# mdrend

`md` is a fast markdown viewer for Linux. It renders files the way GitHub does and shows a tree of the other
markdown files next to them.

## Features

- GitHub look: GitHub's own stylesheet, tables, task lists, footnotes, alerts, autolinks and YAML front matter as a
  table.
- Light and dark themes. The theme follows the system unless you give `--light` or `--dark`.
- File tree of every markdown file under the current directory. Dependency, build and gitignored folders are skipped.
- Live reload: a saved doc reloads, and new docs show up in the tree.
- Keyboard first: move through the tree and scroll the doc without the mouse.
- Web links open in your browser. Local images, videos and PDFs open in their default app.
- Raw HTML renders like on GitHub, but scripts in a doc never run.

## Install

You need Linux, a recent stable Rust toolchain and the WebKitGTK development files. On Debian or Ubuntu:

```sh
sudo apt install libwebkit2gtk-4.1-dev
```

Then install `md` into `~/.cargo/bin`:

```sh
cargo install --git https://github.com/JacoboLansac/mdrend --locked
```

From a clone, `just install` also installs a desktop entry and a dock icon. It needs [just](https://github.com/casey/just).

## Usage

```sh
md README.md     # open a file
md docs/         # open the tree of a folder, with no document
md --dark notes.md
```

The tree lists the markdown files under the current directory, or under the file's folder if the file is outside it.

| Key                    | Action                               |
|------------------------|--------------------------------------|
| `Ctrl+1` or `Esc`      | Focus the tree                       |
| `Ctrl+2`               | Focus the document                   |
| `Ctrl+W` or `Ctrl+Q`   | Quit                                 |
| `Up` / `Down`          | In the tree: show the previous/next doc |
| `Left` / `Right`       | In the tree: fold/unfold a folder    |
| `Enter`                | In the tree: open the doc and focus it |
| `PageUp` / `PageDown`  | In the tree: scroll the doc by a page |
| `Ctrl+Up` / `Ctrl+Down`| In the tree: scroll the doc by a line |

## Development

```sh
just run README.md   # run the dev build
just test            # run the tests
```

## License

[MIT](LICENSE). The bundled GitHub stylesheets come from
[sindresorhus/github-markdown-css](https://github.com/sindresorhus/github-markdown-css), also under the
[MIT license](assets/github-markdown-css.LICENSE).
