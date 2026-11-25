# List recipes
default:
    @just --list

# Install the build dependency (WebKitGTK headers, Debian/Ubuntu)
deps:
    sudo apt install libwebkit2gtk-4.1-dev

# Release build into target/release/md
build:
    cargo build --release

# Build and install `md` into ~/.cargo/bin
install:
    cargo install --path . --locked

# Remove the installed `md`
uninstall:
    cargo uninstall mdreader

# Open a file with the dev build (path relative to where you call just)
[no-cd]
run file *flags:
    cargo run --manifest-path "{{justfile_directory()}}/Cargo.toml" -- {{flags}} "{{file}}"

test:
    cargo test

# Re-download the GitHub markdown stylesheets (MIT, sindresorhus/github-markdown-css)
update-css:
    for f in github-markdown-light.css github-markdown-dark.css; do curl -fsSL "https://raw.githubusercontent.com/sindresorhus/github-markdown-css/main/$f" -o "assets/$f"; done
