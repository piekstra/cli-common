# SPEC v1 conformance checklist

A CLI "conforms to piekstra-cli/1" when every box is checked. Track family
status here.

## Checklist

- [ ] Global `--json` valid on every command; DTO-only stdout; error DTO + exit code on failure
- [ ] Global `-v/--verbose`, `-q/--quiet`, `--no-color`
- [ ] Exit codes 0–6 per SPEC §1.5
- [ ] `auth login` (`--stdin`/`--from-env`/`--no-verify`/`--overwrite`/`--non-interactive`), `auth status` (auth-status/v1), `auth logout [--forget]`, `auth set-credential`
- [ ] `config path|show|set|unset`
- [ ] `self-update [--check] [-y]` with self-update/v1 DTO
- [ ] `completions <shell>`
- [ ] `info` emitting cli-info/v1
- [ ] Secrets only via keychain (`piekstra.<bin>`), stdin, env — never argv
- [ ] ISO `YYYY-MM-DD` accepted on all date flags; `--limit N` on lists
- [ ] Mutations prompt unless `--force`; exit 6 when non-interactive

## Family status

| CLI | Status |
|---|---|
| example-cli | reference implementation |
| xfin | conforms (v0.2.0) — `set-credential` also kept top-level |
| fpl | conforms (v0.2.0) — `init` kept alongside `auth login` |
| lrfl | conforms (v0.2.0) — `config set-account` spelling retained; guest reads need no auth |
| tojfl | conforms (v0.1.x) — SDK keychain service name unchanged; `config set/unset` pending |
| target-cli | planned |
| babylist-cli | planned |
