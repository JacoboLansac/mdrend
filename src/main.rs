use std::borrow::Cow;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use gtk::gio::prelude::*;
use gtk::{gio, glib};
use ignore::gitignore::Gitignore;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::unix::WindowExtUnix;
use tao::window::{Theme, WindowBuilder};
use wry::WebViewBuilderExtUnix;
use wry::http::Response;

const PAGE: &str = include_str!("page.html");
const LIGHT: &str = include_str!("../assets/github-markdown-light.css");
const DARK: &str = include_str!("../assets/github-markdown-dark.css");
const ORIGIN: &str = "md://localhost";
/// CSP for every md:// reply that is not the app page: nothing in it may run or load.
const LOCKED: &str = "default-src 'none'; sandbox";
/// Dependency, build and cache folders: walking them is slow and their docs are not the user's. Gitignored folders
/// are skipped too.
const SKIP_DIRS: [&str; 26] = [
    ".cache", ".cargo", ".direnv", ".git", ".gradle", ".local", ".mypy_cache", ".next", ".npm", ".nuxt", ".pnpm-store",
    ".pytest_cache", ".ruff_cache", ".rustup", ".svelte-kit", ".terraform", ".tox", ".venv", ".yarn", "Pods",
    "__pycache__", "bower_components", "node_modules", "target", "vendor", "venv",
];
/// Walked in full even when gitignored: agents keep their docs there.
const KEEP_DIR: &str = ".claude";
/// Path chars left as-is in URLs.
const URL_SAFE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'/').remove(b'-').remove(b'_').remove(b'.').remove(b'~');

const USAGE: &str = "usage: md [--light|--dark] <file|dir>

Opens <file> rendered GitHub-style. The left tree lists the markdown files
under the current directory (or the file's folder if it is outside it).
Both follow changes on disk: saved docs reload, new docs show up in the tree.
With <dir>, opens the tree of <dir> and no document.
The theme follows the system unless --light or --dark is given.

Keys: Ctrl+1 or Esc tree, Ctrl+2 document, Ctrl+W or Ctrl+Q quit. In the tree:
Up/Down show docs, Left/Right fold folders, Enter opens the doc and moves
focus to it, PageUp/PageDown scroll the doc by a page, Ctrl+Up/Down by a line.";

fn main() {
    let mut theme = None;
    let mut arg_path = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--light" => theme = Some(false),
            "--dark" => theme = Some(true),
            "-h" | "--help" => die(USAGE, 0),
            _ if arg.starts_with('-') => die(&format!("md: unknown option {arg}\n\n{USAGE}"), 2),
            _ => arg_path = Some(arg),
        }
    }
    let Some(arg_path) = arg_path else { die(USAGE, 2) };
    let target = fs::canonicalize(&arg_path)
        .ok()
        .filter(|p| p.is_file() || p.is_dir())
        .unwrap_or_else(|| die(&format!("md: {arg_path}: no such file or directory"), 1));
    let cwd = std::env::current_dir().and_then(fs::canonicalize).unwrap_or_default();
    let root = if target.is_dir() {
        target.clone()
    } else if target.starts_with(&cwd) {
        cwd
    } else {
        target.parent().unwrap().to_path_buf()
    };

    let dark = theme.unwrap_or_else(system_dark);
    // Big folders take a while to walk: walk while GTK and WebKit start, wait for it on the first request.
    let tree_root = root.clone();
    let tree = thread::spawn(move || tree_html(&tree_root).0);
    let watch = Rc::new(Watch { root: root.clone(), ..Default::default() });
    let sent = watch.tree.clone();
    let shell = LazyLock::new(move || {
        let tree = tree.join().unwrap();
        let shell = PAGE
            .replace("{{theme}}", if dark { "dark" } else { "light" })
            .replace("{{css}}", if dark { DARK } else { LIGHT })
            .replace("{{nonce}}", &format!("{:x}", RandomState::new().hash_one(0)))
            .replace("{{tree}}", &tree);
        *sent.lock().unwrap() = tree;
        shell
    });

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(format!("md — {}", name(&target)))
        .with_theme(Some(if dark { Theme::Dark } else { Theme::Light }))
        .with_inner_size(LogicalSize::new(1200.0, 900.0))
        .build(&event_loop)
        .unwrap_or_else(|e| die(&format!("md: cannot open window: {e}"), 1));

    let (nav_root, ipc_watch, load_watch) = (root.clone(), watch.clone(), watch.clone());
    let url = format!("{ORIGIN}/{}", href(target.strip_prefix(&root).unwrap())); // a dir opens md://localhost/
    if target.is_file() {
        watch_doc(&watch, &url); // before the page is served, so no save goes unseen
    }
    // ponytail: Linux only (build_gtk, xdg-open, md:// origin); macOS needs build(&window) and `open`.
    let webview = wry::WebViewBuilder::new()
        .with_custom_protocol("md".into(), move |_, req| serve(&root, &shell, req.uri().path(), req.uri().query()))
        .with_navigation_handler(move |url| navigate(&nav_root, &url))
        .with_ipc_handler(move |req| match req.body().as_str() {
            "quit" => exit(0), // Ctrl+W, Ctrl+Q
            url => match url.strip_prefix("watch ") {
                Some(doc) => watch_doc(&ipc_watch, doc), // the page shows another doc
                None => open_link(&ipc_watch.root, url),
            },
        })
        .with_url(url)
        .with_on_page_load_handler(move |e, url| {
            if matches!(e, wry::PageLoadEvent::Finished) {
                // A reloaded page shows its URL's doc and the startup tree: watch that doc, send the latest tree.
                if load_watch.loaded.replace(true) {
                    watch_doc(&load_watch, &url);
                    load_watch.tree.lock().unwrap().clear();
                }
                rescan(&load_watch)
            }
        })
        .with_background_color(if dark { (13, 17, 23, 255) } else { (255, 255, 255, 255) }) // no white flash before the first paint
        .build_gtk(window.default_vbox().unwrap())
        .unwrap_or_else(|e| die(&format!("md: cannot create webview: {e}"), 1));
    let _ = watch.webview.set(webview);

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            let _ = &window; // keep the window alive for the loop's lifetime
            *flow = ControlFlow::Exit;
        }
    });
}

fn die(msg: &str, code: i32) -> ! {
    if code == 0 { println!("{msg}") } else { eprintln!("{msg}") }
    exit(code)
}

/// Asks the XDG desktop portal (GNOME, KDE, ...) for the color scheme: 1 = dark.
fn system_dark() -> bool {
    use gtk::glib::{VariantTy, prelude::ToVariant};
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return false };
    bus.call_sync(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
        "ReadOne",
        Some(&("org.freedesktop.appearance", "color-scheme").to_variant()),
        Some(VariantTy::new("(v)").unwrap()),
        gio::DBusCallFlags::NONE,
        1000,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|r| r.child_value(0).as_variant()?.get::<u32>())
        == Some(1)
}

/// Live reload, from inotify through GIO, so an idle viewer does no work. The open doc has its own watch and
/// reloads as soon as a save is done. Every tree folder has one too: new, deleted or renamed docs and folders
/// rebuild the tree a second later.
// ponytail: one inotify watch per folder; a root with more folders than fs.inotify.max_user_watches loses the rest.
#[derive(Default)]
struct Watch {
    root: PathBuf,
    webview: OnceCell<wry::WebView>,
    doc: RefCell<Option<gio::FileMonitor>>,
    folders: RefCell<HashMap<PathBuf, gio::FileMonitor>>,
    tree: Arc<Mutex<String>>, // the page's tree; the first one comes with the page, from the startup walk
    pending: Cell<bool>,      // a rescan is scheduled
    walking: Cell<bool>,      // one walk at a time
    again: Cell<bool>,        // changes came during the walk: walk again after it
    loaded: Cell<bool>,       // the page loaded once
}

const MOVES: gio::FileMonitorFlags = gio::FileMonitorFlags::WATCH_MOVES;

/// Watches the doc at an md:// URL; its saves reload the page's doc.
fn watch_doc(w: &Rc<Watch>, url: &str) {
    let w2 = w.clone();
    let doc = url_path(&w.root, url).filter(|p| p.is_file());
    *w.doc.borrow_mut() = doc.and_then(|doc| on_save(doc, move || run_js(&w2, "reload()")));
}

fn on_save(doc: PathBuf, f: impl Fn() + 'static) -> Option<gio::FileMonitor> {
    let m = gio::File::for_path(&doc).monitor_file(MOVES, gio::Cancellable::NONE).ok()?;
    m.connect_changed(move |_, _, other, e| {
        if saved(&doc, e, other.and_then(|o| o.path()).as_deref()) {
            f()
        }
    });
    Some(m)
}

/// Whether an event on a doc's watch is a finished save: written in place (GIO tells once the writer closes the
/// file), or moved or renamed over it (vim, most editors, agent tools). Not `Created`: it comes before the content.
fn saved(doc: &Path, e: gio::FileMonitorEvent, other: Option<&Path>) -> bool {
    use gio::FileMonitorEvent as E;
    match e {
        E::ChangesDoneHint | E::MovedIn => true,
        E::Renamed => other == Some(doc), // not when the doc itself is renamed away
        _ => false,
    }
}

/// Whether a folder event can change the tree: a doc or a folder came, went or was renamed.
/// A saved .gitignore can show or hide folders.
fn changes_tree(e: gio::FileMonitorEvent, mut paths: impl Iterator<Item = PathBuf>, watched: impl Fn(&Path) -> bool) -> bool {
    use gio::FileMonitorEvent as E;
    let moved = matches!(e, E::Created | E::Deleted | E::MovedIn | E::MovedOut | E::Renamed);
    paths.any(|p| {
        moved && (is_md(&p) || p.is_dir() || watched(&p)) // a gone folder is no dir any more, but was watched
            || (moved || e == E::ChangesDoneHint) && p.ends_with(".gitignore")
    })
}

/// Rescans a second after the first change of a burst.
fn schedule(w: &Rc<Watch>) {
    if !w.pending.replace(true) {
        let w = w.clone();
        glib::timeout_add_local_once(Duration::from_secs(1), move || rescan(&w));
    }
}

/// Walks the tree off the main thread, watches the new folders, drops the gone ones, and sends the page the tree if
/// it changed.
fn rescan(w: &Rc<Watch>) {
    w.pending.set(false);
    if w.walking.replace(true) {
        w.again.set(true);
        return;
    }
    let w = w.clone();
    glib::spawn_future_local(async move {
        let root = w.root.clone();
        let walked = gio::spawn_blocking(move || tree_html(&root)).await;
        w.walking.set(false);
        if w.again.replace(false) {
            schedule(&w);
        }
        let Ok((tree, dirs)) = walked else { return };
        let dirs: HashSet<PathBuf> = dirs.into_iter().collect();
        let mut folders = w.folders.borrow_mut();
        folders.retain(|d, _| dirs.contains(d));
        let mut added = false;
        for d in dirs {
            let Entry::Vacant(slot) = folders.entry(d) else { continue };
            let Ok(m) = gio::File::for_path(slot.key()).monitor_directory(MOVES, gio::Cancellable::NONE) else { continue };
            let w = w.clone();
            m.connect_changed(move |_, f, other, e| {
                let paths = [Some(f), other].into_iter().flatten().filter_map(|f| f.path());
                if changes_tree(e, paths, |p| w.folders.borrow().contains_key(p)) {
                    schedule(&w)
                }
            });
            slot.insert(m);
            added = true;
        }
        drop(folders);
        if added {
            schedule(&w); // a new folder had no watch while the walk read it: walk again for docs made in it meanwhile
        }
        let mut sent = w.tree.lock().unwrap();
        if *sent != tree {
            run_js(&w, &format!("setTree('{}')", utf8_percent_encode(&tree, NON_ALPHANUMERIC))); // only [A-Za-z0-9%]
            *sent = tree;
        }
    });
}

fn run_js(w: &Watch, js: &str) {
    if let Some(Err(e)) = w.webview.get().map(|v| v.evaluate_script(js)) {
        eprintln!("md: live reload failed: {e}");
    }
}

/// Answers every md:// request: docs as rendered pages, images as raw bytes.
fn serve(root: &Path, shell: &str, uri_path: &str, query: Option<&str>) -> Response<Cow<'static, [u8]>> {
    let reply = |status: u16, ctype: &str, body: Vec<u8>| {
        let r = Response::builder().status(status).header("Content-Type", ctype);
        // The app page carries its own CSP (meta tag, with the script nonce).
        let r = if ctype.starts_with("text/html") { r } else { r.header("Content-Security-Policy", LOCKED) };
        r.body(Cow::Owned(body)).unwrap()
    };
    let Some(path) = local_path(root, uri_path).filter(|p| p.is_file() || p == root) else {
        return reply(404, "text/plain", b"Not found".to_vec());
    };
    let body = if path == root {
        "<p>No document open. Pick one in the tree.</p>".to_owned()
    } else if let Some(ctype) = asset_type(&path) {
        return match fs::read(&path) {
            Ok(bytes) => reply(200, ctype, bytes),
            Err(e) => reply(500, "text/plain", e.to_string().into_bytes()),
        };
    } else {
        render(&path)
    };
    if query == Some("body") {
        // Plain text so a page load of this URL can't run the doc's HTML outside the app's CSP.
        return reply(200, "text/plain; charset=utf-8", body.into_bytes());
    }
    let title = esc(&name(&path));
    // Content goes in last so text inside the doc can't fill the other placeholders.
    let page = shell.replace("{{title}}", &title).replace("{{content}}", &body);
    reply(200, "text/html; charset=utf-8", page.into_bytes())
}

/// Allows page loads of root (no doc open) and of docs under it. Clicked links never get here: the page's click
/// handler routes them, so a doc can't launch anything on its own (e.g. with a meta refresh).
fn navigate(root: &Path, url: &str) -> bool {
    url == "about:blank"
        || !url.contains('?') && url_path(root, url).is_some_and(|p| p == root || p.is_file() && asset_type(&p).is_none())
}

/// Clicked links the viewer can't show: web links go to the browser, local images/PDFs to their app.
fn open_link(root: &Path, url: &str) {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
        open_external(url.as_ref());
    } else if let Some(p) = url_path(root, url).filter(|p| asset_type(p).is_some()) {
        open_external(p.as_os_str());
    }
}

fn open_external(target: &std::ffi::OsStr) {
    if let Err(e) = Command::new("xdg-open").arg(target).spawn() {
        eprintln!("md: xdg-open failed: {e}");
    }
}

/// Maps an md:// URL to an existing path under root.
fn url_path(root: &Path, url: &str) -> Option<PathBuf> {
    local_path(root, url.strip_prefix(ORIGIN)?.split(['?', '#']).next()?)
}

/// Maps a URL path to an existing path under root; `..` and symlinks that lead out are rejected.
fn local_path(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let rel = percent_decode_str(uri_path).decode_utf8_lossy();
    fs::canonicalize(root.join(rel.trim_start_matches('/'))).ok().filter(|p| p.starts_with(root))
}

/// Served raw instead of rendered.
fn asset_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

fn render(path: &Path) -> String {
    let md = fs::read_to_string(path).unwrap_or_else(|e| format!("**Cannot read `{}`:** {e}", path.display()));
    let (meta, md) = front_matter(&md);
    let mut o = comrak::Options::default();
    let x = &mut o.extension;
    (x.table, x.strikethrough, x.autolink, x.tasklist, x.footnotes, x.alerts) = (true, true, true, true, true, true);
    x.header_id_prefix = Some(String::new());
    o.render.tasklist_classes = true; // GitHub CSS hides the bullets of task lists
    o.render.r#unsafe = true; // raw HTML like GitHub; the page's CSP keeps its scripts from running
    meta + &tagfilter(&comrak::markdown_to_html(md, &o))
}

/// GitHub shows YAML front matter as a table above the doc instead of rendering it as markdown.
/// Returns that table and the markdown after the front matter.
// ponytail: top-level `key: value` pairs only; nested lines are appended to the previous value.
fn front_matter(md: &str) -> (String, &str) {
    let plain = || (String::new(), md);
    let mut lines = md.split_inclusive('\n');
    let Some(first) = lines.next().filter(|l| l.trim_end() == "---") else { return plain() };
    let mut end = first.len();
    let mut rows: Vec<(&str, String)> = Vec::new();
    for line in lines {
        end += line.len();
        let t = line.trim();
        if t == "---" {
            if rows.is_empty() {
                return plain();
            }
            let (head, body): (String, String) = rows
                .iter()
                .map(|(k, v)| {
                    let v = v.trim();
                    let v = ['"', '\''].iter().find_map(|q| v.strip_prefix(*q)?.strip_suffix(*q)).unwrap_or(v); // one pair of quotes
                    (format!("<th>{}</th>", esc(k)), format!("<td>{}</td>", esc(v)))
                })
                .unzip();
            return (format!("<table><thead><tr>{head}</tr></thead><tbody><tr>{body}</tr></tbody></table>\n"), &md[end..]);
        }
        if t.is_empty() || t.starts_with('#') {
            continue; // blank or YAML comment
        }
        // `- item` at column 0 only continues a list value (`tools:` then `- Read`).
        let list_item = line.starts_with('-') && rows.last().is_some_and(|(_, v)| v.trim().is_empty() || v.trim().starts_with('-'));
        let indented = line.starts_with([' ', '\t']) || list_item;
        match (indented, line.split_once(':'), rows.last_mut()) {
            // A key needs `: ` or `:` at line end, so URLs and times in prose don't count.
            (false, Some((k, v)), _) if v.is_empty() || v.starts_with(char::is_whitespace) => rows.push((k.trim(), v.to_owned())),
            (true, _, Some((_, v))) => {
                v.push(' ');
                v.push_str(t);
            }
            _ => return plain(), // prose between two rules, not YAML
        }
    }
    plain() // never closed: not front matter
}

/// GFM's tagfilter (comrak dropped it): escapes raw tags that would swallow the rest of the page.
/// Safe on the whole output: comrak already escapes `<` in text and code.
fn tagfilter(html: &str) -> String {
    const TAGS: [&str; 9] = ["title", "textarea", "style", "xmp", "iframe", "noembed", "noframes", "script", "plaintext"];
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let name = rest.strip_prefix('/').unwrap_or(rest);
        let hit = TAGS.iter().any(|t| {
            name.get(..t.len()).is_some_and(|n| n.eq_ignore_ascii_case(t))
                && name[t.len()..].chars().next().is_none_or(|c| c.is_ascii_whitespace() || c == '>' || c == '/')
        });
        out.push_str(if hit { "&lt;" } else { "<" });
    }
    out.push_str(rest);
    out
}

fn is_md(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// The tree's HTML and every folder walked, for the live reload watches.
fn tree_html(root: &Path) -> (String, Vec<PathBuf>) {
    let (mut out, mut dirs) = (String::new(), Vec::new());
    let mut rules = parent_rules(root, &mut dirs);
    walk(root, root, &mut out, &mut dirs, &mut rules, obeys(root));
    (format!("<ul>{out}</ul>"), dirs)
}

/// Whether gitignore rules apply in dir: not in or below KEEP_DIR, unless a repo (a worktree) starts below it.
fn obeys(dir: &Path) -> bool {
    for d in dir.ancestors() {
        if d.join(".git").exists() {
            return true;
        }
        if d.ends_with(KEEP_DIR) {
            return false;
        }
    }
    true
}

/// The .gitignore rules of root's parent folders in the same repo, outermost first. Adds those folders to `dirs`,
/// so saving one of their .gitignore files rebuilds the tree too.
fn parent_rules(root: &Path, dirs: &mut Vec<PathBuf>) -> Vec<Gitignore> {
    let Some(repo) = root.ancestors().find(|d| d.join(".git").exists()) else { return Vec::new() };
    let mut rules = Vec::new();
    for d in root.ancestors().skip(1).take_while(|d| d.starts_with(repo)) {
        dirs.push(d.to_path_buf());
        if d.join(".gitignore").is_file() {
            rules.push(Gitignore::new(d.join(".gitignore")).0);
        }
    }
    rules.reverse();
    rules
}

/// Whether the innermost .gitignore rule that matches the folder ignores it.
fn ignored(rules: &[Gitignore], dir: &Path) -> bool {
    rules.iter().rev().map(|g| g.matched(dir, true)).find(|m| !m.is_none()).is_some_and(|m| m.is_ignore())
}

/// Appends `<li>`s for dir's markdown files and for subfolders that contain some.
/// Returns false when nothing was found. `rules` holds the .gitignore files that apply to dir, outermost first;
/// they skip folders only, so gitignored docs (private notes, plans) still show.
// ponytail: walks the whole tree at startup; lazy per-folder loading if huge dirs get slow.
fn walk(root: &Path, dir: &Path, out: &mut String, dirs: &mut Vec<PathBuf>, rules: &mut Vec<Gitignore>, obey: bool) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    dirs.push(dir.to_path_buf());
    let entries: Vec<_> = entries.flatten().filter_map(|e| Some((e.file_type().ok()?.is_dir(), e.path()))).collect();
    let has = |n: &str| entries.iter().any(|(_, p)| name(p) == n);
    // A repo's own rules start fresh (an outer repo's don't reach in); none apply in or below KEEP_DIR, except in
    // a repo there (a worktree).
    let mut fresh = Vec::new();
    let repo = has(".git");
    let rules = if repo { &mut fresh } else { rules };
    let obey = repo || obey && !dir.ends_with(KEEP_DIR);
    let own = obey && has(".gitignore");
    if own {
        rules.push(Gitignore::new(dir.join(".gitignore")).0);
    }
    let mut entries: Vec<_> = entries.into_iter().filter(|(_, p)| !SKIP_DIRS.contains(&name(p).as_ref())).collect();
    // Folders first, then case-insensitive by name.
    entries.sort_by_key(|(is_dir, p)| (!is_dir, p.file_name().unwrap().to_string_lossy().to_lowercase()));
    let start = out.len();
    for (is_dir, path) in entries {
        let name = esc(&name(&path));
        if is_dir {
            if obey && !path.ends_with(KEEP_DIR) && ignored(rules, &path) {
                continue;
            }
            let mut sub = String::new();
            if walk(root, &path, &mut sub, dirs, rules, obey) {
                out.push_str(&format!("<li><details><summary>{name}</summary><ul>{sub}</ul></details></li>"));
            }
        } else if is_md(&path) {
            let link = href(path.strip_prefix(root).unwrap());
            out.push_str(&format!("<li><a href=\"/{link}\">{name}</a></li>"));
        }
    }
    if own {
        rules.pop();
    }
    out.len() > start
}

/// Last path component; `/` for the filesystem root.
fn name(path: &Path) -> Cow<'_, str> {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
}

fn href(rel: &Path) -> String {
    utf8_percent_encode(&rel.to_string_lossy(), URL_SAFE).to_string()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_paths_serve_navigate() {
        let root = std::env::temp_dir().join(format!("md-test-{}", std::process::id()));
        for d in ["b/deep", "a", "empty", ".hidden", ".git", "node_modules"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["Z.md", "b/deep/x.markdown", "a/<y>.md", "a/notes.txt", ".hidden/h.md", ".git/g.md", "node_modules/n.md", "i.png"] {
            fs::write(root.join(f), "# hi").unwrap();
        }
        let root = fs::canonicalize(&root).unwrap();
        // Folders first and sorted (hidden ones too), files escaped and URL-encoded, junk skipped.
        let (tree, dirs) = tree_html(&root);
        assert_eq!(
            tree,
            "<ul><li><details><summary>.hidden</summary><ul><li><a href=\"/.hidden/h.md\">h.md</a></li></ul></details></li>\
             <li><details><summary>a</summary><ul><li><a href=\"/a/%3Cy%3E.md\">&lt;y&gt;.md</a></li></ul></details></li>\
             <li><details><summary>b</summary><ul><li><details><summary>deep</summary><ul>\
             <li><a href=\"/b/deep/x.markdown\">x.markdown</a></li></ul></details></li></ul></details></li>\
             <li><a href=\"/Z.md\">Z.md</a></li></ul>"
        );
        // Watched for live reload: every walked folder, also those with no docs yet.
        assert_eq!(dirs, ["", ".hidden", "a", "b", "b/deep", "empty"].map(|d| root.join(d)));
        std::os::unix::fs::symlink("/etc/passwd", root.join("link.md")).unwrap();

        // Nothing outside root is reachable.
        assert_eq!(local_path(&root, "/a/%3Cy%3E.md"), Some(root.join("a/<y>.md")));
        assert_eq!(local_path(&root, "//Z.md"), Some(root.join("Z.md")));
        assert_eq!(local_path(&root, "/a/../../../../../../etc/passwd"), None);
        assert_eq!(local_path(&root, "/%2e%2e/%2e%2e/%2e%2e/%2e%2e/%2e%2e/etc/passwd"), None);
        assert_eq!(local_path(&root, "/link.md"), None);

        // Only the app page is HTML; every other reply is locked down.
        let csp = |r: &Response<Cow<[u8]>>| r.headers().get("Content-Security-Policy").map(|v| v.to_str().unwrap().to_owned());
        let page = serve(&root, PAGE, "/Z.md", None);
        let html = String::from_utf8_lossy(page.body());
        assert!(html.contains("http-equiv=\"Content-Security-Policy\"") && html.contains("<h1 id=\"hi\">hi"), "{html}");
        let home = String::from_utf8_lossy(serve(&root, PAGE, "/", None).body()).into_owned();
        assert!(home.contains("No document open") && !home.contains("{{content}}"), "{home}");
        assert_eq!(serve(&root, PAGE, "/a", None).status(), 404);
        let body = serve(&root, PAGE, "/Z.md", Some("body"));
        assert_eq!(body.headers()["Content-Type"], "text/plain; charset=utf-8");
        for r in [body, serve(&root, PAGE, "/i.png", None), serve(&root, PAGE, "/link.md", None)] {
            assert_eq!(csp(&r).as_deref(), Some(LOCKED));
        }

        // Page loads: docs under root only.
        let nav = |u: &str| navigate(&root, u);
        assert!(nav("md://localhost/Z.md") && nav("md://localhost/a/%3Cy%3E.md#x") && nav("about:blank") && nav("md://localhost/"));
        assert!(!nav("md://localhost/Z.md?body") && !nav("md://localhost/i.png") && !nav("md://localhost/link.md"));
        assert!(!nav("https://example.com") && !nav("md://localhost/a") && !nav("javascript:alert(1)"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn doc_watch_reloads_finished_saves_only() {
        let dir = std::env::temp_dir().join(format!("md-watch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dir = fs::canonicalize(&dir).unwrap();
        let doc = dir.join("d.md");
        fs::write(&doc, "v1").unwrap();
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            let seen = Rc::new(RefCell::new(Vec::new())); // the doc's content at each reload
            let (s, d) = (seen.clone(), doc.clone());
            let _watch = on_save(doc.clone(), move || s.borrow_mut().push(fs::read_to_string(&d).unwrap_or_default())).unwrap();
            let settle = || {
                let end = std::time::Instant::now() + Duration::from_millis(300);
                while std::time::Instant::now() < end {
                    while ctx.iteration(false) {}
                    thread::sleep(Duration::from_millis(5));
                }
            };
            let last = || seen.borrow().last().cloned().unwrap_or_default();
            settle();

            fs::write(&doc, "v2").unwrap(); // in place
            settle();
            assert_eq!(last(), "v2");
            fs::write(dir.join(".d.tmp"), "v3").unwrap(); // rename over: vim, editors, agent tools
            fs::rename(dir.join(".d.tmp"), &doc).unwrap();
            settle();
            assert_eq!(last(), "v3");
            fs::remove_file(&doc).unwrap(); // deleted, then recreated slowly: no reload until the writer is done
            let mut f = fs::File::create(&doc).unwrap();
            std::io::Write::write_all(&mut f, b"v").unwrap();
            settle();
            assert_eq!(last(), "v3");
            std::io::Write::write_all(&mut f, b"4").unwrap();
            drop(f);
            settle();
            assert_eq!(last(), "v4");
            let n = seen.borrow().len();
            fs::rename(&doc, dir.join("d.md~")).unwrap(); // renamed away (vim backup), another file saved
            fs::write(dir.join("other.md"), "x").unwrap();
            settle();
            assert_eq!(seen.borrow().len(), n);
            fs::write(&doc, "v5").unwrap(); // then the new doc written
            settle();
            assert_eq!(last(), "v5");
        })
        .unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tree_events_need_a_doc_or_folder() {
        use gio::FileMonitorEvent as E;
        let dir = std::env::temp_dir().join(format!("md-events-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("4913"), "").unwrap(); // vim's write test file
        let hit = |e, p: &str, watched: bool| changes_tree(e, std::iter::once(dir.join(p)), |_| watched);
        assert!(hit(E::Created, "x.md", false) && hit(E::Deleted, "x.MARKDOWN", false) && hit(E::Renamed, "x.md", false));
        assert!(hit(E::Created, "sub", false) && hit(E::MovedOut, "gone", true));
        assert!(!hit(E::Created, "4913", false) && !hit(E::Deleted, "gone", false) && !hit(E::ChangesDoneHint, "x.md", false));
        assert!(hit(E::ChangesDoneHint, ".gitignore", false) && !hit(E::Changed, ".gitignore", false));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gitignore_skips_folders_not_docs() {
        let root = std::env::temp_dir().join(format!("md-gitignore-{}", std::process::id()));
        for d in [".git", "gen", ".claude/plans/tmp", ".claude/worktrees/wt/build", ".claude/worktrees/wt/tmp/build", "sub/local", "sub/wl"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for d in ["repo/.git", "repo/gen", "vendor", "docs/gen"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join(".gitignore"), "gen/\nwl/\ntmp/\n.claude/\n.claude/*\nsecret.md\n").unwrap();
        fs::write(root.join("sub/.gitignore"), "local/\n!wl/\n").unwrap(); // the inner file wins
        fs::write(root.join(".claude/.gitignore"), "*\n!.gitignore\n").unwrap();
        fs::write(root.join(".claude/worktrees/wt/.git"), "gitdir: x").unwrap(); // a worktree obeys its own rules
        fs::write(root.join(".claude/worktrees/wt/.gitignore"), "build/\n").unwrap();
        for f in [".claude/worktrees/wt/build/b.md", ".claude/worktrees/wt/tmp/t2.md", ".claude/worktrees/wt/tmp/build/b2.md"] {
            fs::write(root.join(f), "# hi").unwrap();
        }
        for f in ["gen/g.md", ".claude/plans/tmp/t.md", "secret.md", "sub/local/l.md", "sub/s.md", "sub/wl/w.md", "repo/gen/r.md"] {
            fs::write(root.join(f), "# hi").unwrap();
        }
        for f in ["vendor/v.md", "docs/gen/d.md"] {
            fs::write(root.join(f), "# hi").unwrap();
        }
        let root = fs::canonicalize(&root).unwrap();
        let docs = |tree: &str| tree.split("href=\"").skip(1).map(|s| s.split('"').next().unwrap().to_owned()).collect::<Vec<_>>();
        // Gitignored and listed folders are skipped at any depth; gitignored docs, .claude and nested repos are not.
        let (tree, dirs) = tree_html(&root);
        let want = ["/.claude/plans/tmp/t.md", "/.claude/worktrees/wt/tmp/t2.md", "/repo/gen/r.md", "/sub/wl/w.md", "/sub/s.md", "/secret.md"];
        assert_eq!(docs(&tree), want);
        for skipped in ["gen", "sub/local", "vendor", "docs/gen"] {
            assert!(!dirs.contains(&root.join(skipped)), "{skipped}");
        }
        // A root inside the repo still follows the repo's .gitignore and watches its folder; not below .claude.
        let (tree, dirs) = tree_html(&root.join("docs"));
        assert!(tree == "<ul></ul>" && dirs[0] == root, "{tree}");
        assert_eq!(docs(&tree_html(&root.join(".claude/plans")).0), ["/tmp/t.md"]);
        assert_eq!(docs(&tree_html(&root.join(".claude/worktrees/wt/tmp")).0), ["/t2.md"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn front_matter_becomes_table() {
        let md = "---\nname: x\ndescription: \"a <b> 'c'\"\n# comment\ntools:\n- Read\n- Write\n---\n# Doc\n";
        let table = "<table><thead><tr><th>name</th><th>description</th><th>tools</th></tr></thead>\
                     <tbody><tr><td>x</td><td>a &lt;b&gt; 'c'</td><td>- Read - Write</td></tr></tbody></table>\n";
        assert_eq!(front_matter(md), (table.to_owned(), "# Doc\n"));
        let plain = [
            "# No front matter\n",
            "---",
            "---\n---\nhi",
            "---\nnever: closed\n",
            "text\n---\n",
            "---\nA paragraph\n---\n",
            "---\n# Title\n---\n",
            "---\nSee https://example.com\n---\n",
            "---\nMeeting at 10:30 today\n---\n",
            "---\nStatus: draft\n\nProse here.\n---\n",
            "---\nStatus: draft\n\n# Heading\n\n- item\n---\n",
        ];
        for plain in plain {
            assert_eq!(front_matter(plain), (String::new(), plain));
        }
    }

    #[test]
    fn tagfilter_escapes_page_swallowing_tags() {
        assert_eq!(
            tagfilter("<p>a <script>x</script> <scripts> <STYLE>\n<div title=\"t\"><textarea/>é<"),
            "<p>a &lt;script>x&lt;/script> <scripts> &lt;STYLE>\n<div title=\"t\">&lt;textarea/>é<"
        );
    }
}
