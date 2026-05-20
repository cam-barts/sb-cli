## v1.4.1 (2026-05-20)

### Fix

- **ci**: unset persisted GITHUB_TOKEN on coverage checkout (#3)

## v1.4.0 (2026-05-20)

### Feat

- **config**: treat ~/.config/sb/config.toml as a full fallback config (#2)

## v1.3.2 (2026-05-19)

### Fix

- **daily,page**: route writes through content_dir, not space_root (#1)

## v1.3.1 (2026-05-18)

### Fix

- **clippy**: use sort_by_key for Date keys (rustc 1.95 lint)

## v1.3.0 (2026-05-18)

### Feat

- extend sb daily into a jrnl-flavoured journal command

## v1.2.0 (2026-05-14)

### Feat

- port logs/screenshot/describe + auto-TTY output mode

## v1.1.0 (2026-04-22)

### Feat

- respect XDG_CONFIG_HOME so sync works everywhere
