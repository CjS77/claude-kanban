# Developer tasks. `cargo build` alone produces a working plugin — nothing here is required for that; these targets exist
# for regenerating committed artifacts and day-to-day hygiene.

# The Tailwind v4 STANDALONE CLI (single binary, no node). Built and tested with v4.3.x:
#   https://github.com/tailwindlabs/tailwindcss/releases  →  tailwindcss-linux-x64 / -macos-arm64 / …
# Point TAILWIND at the downloaded binary, or put `tailwindcss` on PATH.
tailwind := env_var_or_default("TAILWIND", "tailwindcss")

default: test

# Regenerate assets/app.css (COMMITTED) after changing templates, glue.js, or the css source.
css:
    {{ tailwind }} -i css/app.tailwind.css -o assets/app.css --minify

# Rebuild CSS on every template edit while hacking on the UI.
css-watch:
    {{ tailwind }} -i css/app.tailwind.css -o assets/app.css --watch

test:
    cargo test

lint:
    cargo clippy --all-targets

# Formatting uses unstable rustfmt options — nightly only. (Per project convention, don't run this as part of committing.)
fmt:
    cargo +nightly fmt

# Cut a release: check that plugin.json, marketplace.json, and Cargo.toml agree on <version>, then create the tag
# v<version>. Deliberately never pushes — `git push origin v<version>` is the explicit step that fires the release
# workflow (.github/workflows/release.yml), which builds the binaries and attaches them to a GitHub Release.
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    v="{{ version }}"
    cargo_v=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
    plugin_v=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' .claude-plugin/plugin.json | head -n1)
    market_v=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' .claude-plugin/marketplace.json | head -n1)
    for pair in "Cargo.toml:$cargo_v" ".claude-plugin/plugin.json:$plugin_v" ".claude-plugin/marketplace.json:$market_v"; do
        [ "${pair#*:}" = "$v" ] || { echo "${pair%%:*} says '${pair#*:}', not '$v' — bump the manifests in lockstep first" >&2; exit 1; }
    done
    git tag "v$v"
    echo "Tagged v$v. Push it to publish the release: git push origin v$v"
