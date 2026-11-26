icons := home_directory() / ".local/share/icons/hicolor/scalable/apps"
# GNOME Shell caches icons by path, so the installed name changes with the content.
icon := icons / "mdrender-" + replace_regex(sha256_file(justfile_directory() / "assets/mdrender.svg"), '^(.{8}).*$', '$1') + ".svg"

# List recipes
default:
    @just --list

# Install the build dependency (WebKitGTK headers, Debian/Ubuntu)
deps:
    sudo apt install libwebkit2gtk-4.1-dev

# Release build into target/release/md
build:
    cargo build --release

# Build and install `md` into ~/.cargo/bin, with its desktop entry and dock icon
install:
    cargo install --path . --locked
    rm -f "{{icons}}"/mdrender.svg "{{icons}}"/mdrender-????????.svg
    install -Dm644 assets/mdrender.svg "{{icon}}"
    mkdir -p ~/.local/share/applications
    sed -e "s|^Exec=md|Exec=${CARGO_HOME:-$HOME/.cargo}/bin/md|" -e "s|^Icon=.*|Icon={{icon}}|" assets/mdrender.desktop > ~/.local/share/applications/mdrender.desktop

# Remove the installed `md`, its desktop entry and icon
uninstall:
    rm -f ~/.local/share/applications/mdrender.desktop "{{icons}}"/mdrender.svg "{{icons}}"/mdrender-????????.svg
    cargo uninstall mdrender

# Open a file with the dev build (path relative to where you call just)
[no-cd]
run file *flags:
    cargo run --manifest-path "{{justfile_directory()}}/Cargo.toml" -- {{flags}} "{{file}}"

test:
    cargo test

# Re-download the GitHub markdown stylesheets (MIT, sindresorhus/github-markdown-css)
update-css:
    for f in github-markdown-light.css github-markdown-dark.css; do curl -fsSL "https://raw.githubusercontent.com/sindresorhus/github-markdown-css/main/$f" -o "assets/$f"; done
