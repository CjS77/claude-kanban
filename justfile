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
