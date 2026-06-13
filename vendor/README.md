# Vendored GOWA

GOWA (`aldinokemal/go-whatsapp-web-multidevice`) is vendored as a **git submodule** pinned to a
release tag, not modified in place. This directory holds the pin and any local patch series.

## Pin

- **Upstream:** https://github.com/aldinokemal/go-whatsapp-web-multidevice
- **Tag:** `v8.7.0`
- **License:** MIT
- **Built with:** Go 1.23 (pin the exact toolchain in CI for reproducibility)

## Adding the submodule (run once, on a machine with network)

```sh
git submodule add https://github.com/aldinokemal/go-whatsapp-web-multidevice vendor/gowa
git -C vendor/gowa fetch --tags
git -C vendor/gowa checkout v8.7.0
git add .gitmodules vendor/gowa
git commit -m "vendor: pin GOWA at v8.7.0"
```

## Local patches

Keep any local changes (e.g. surfacing `contextInfo.mentionedJid` if true @-mention is ever needed)
as a **thin commit series on top of the tag** inside `vendor/gowa`, recorded here:

| patch | purpose | status |
|-------|---------|--------|
| _(none yet)_ | — | "mention" is reply-to-bot in the shim; no GOWA patch required |

When bumping the tag: rebase the patch series onto the new tag, rebuild the CI artifact, and refresh
the golden fixtures in `tests/fixtures` against the new payload shapes.

## Build → artifact (CI, not per-box)

CI builds the pinned source once into a versioned binary; `deploy/provision.sh` installs that
prebuilt binary. The Go toolchain never lands on a prod box (smaller image, smaller attack surface).
Reproducibility comes from the pinned tag + pinned Go version.

```sh
# CI build step (illustrative)
cd vendor/gowa/src
CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o gowa-v8.7.0-linux-amd64 .
```
