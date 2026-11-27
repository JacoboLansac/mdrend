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

# Bump the version (patch, minor, major or X.Y.Z), test, tag, publish to crates.io and push. Run on master.
publish level="patch":
    #!/usr/bin/env bash
    set -euo pipefail
    [ "$(git branch --show-current)" = master ] || { echo "publish: not on master"; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo "publish: uncommitted changes"; exit 1; }
    git fetch -q origin master
    git merge-base --is-ancestor origin/master HEAD || { echo "publish: behind origin/master, pull first"; exit 1; }
    cur=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
    IFS=. read -r ma mi pa <<< "$cur"
    case "{{level}}" in
        major) new="$((ma + 1)).0.0" ;;
        minor) new="$ma.$((mi + 1)).0" ;;
        patch) new="$ma.$mi.$((pa + 1))" ;;
        *) new="{{level}}" ;;
    esac
    [[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "publish: {{level}} is not patch, minor, major or X.Y.Z"; exit 1; }
    read -rp "Publish mdrender $cur -> $new to crates.io? It cannot be undone. [y/N] " ok
    [ "$ok" = y ] || exit 1
    # Same version: publish the current one as is (the first release).
    if [ "$new" != "$cur" ]; then
        sed -i "0,/^version = \".*\"/s//version = \"$new\"/" Cargo.toml
        cargo update --workspace --offline --quiet
    fi
    cargo test --locked
    if [ "$new" != "$cur" ]; then git commit -qm "chore: release v$new" Cargo.toml Cargo.lock; fi
    cargo publish --locked
    # Tagged after the publish, so a failed publish can rerun with `just publish X.Y.Z`.
    git tag "v$new"
    git push origin master "v$new"

# Re-download the GitHub markdown stylesheets (MIT, sindresorhus/github-markdown-css)
update-css:
    for f in github-markdown-light.css github-markdown-dark.css; do curl -fsSL "https://raw.githubusercontent.com/sindresorhus/github-markdown-css/main/$f" -o "assets/$f"; done
