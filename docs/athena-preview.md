# Athena read-only init preview

This page records the non-root `nut init --dry-run` preview against athena.
It is not an apply. Root dry-run and apply remain the adopt phase's
responsibility.

## Provenance

- Host: `athena` over Tailscale SSH (`just remote athena …`).
- Config source: the redacted in-tree fixture
  `tests/fixtures/hosts/athena/nut.toml`. Mail addresses use
  `example.invalid`. `POWEROFF_WAIT` stays at the deployed `30m` so golden
  parity matches live files.
- Staging: copied to `athena:~/.cache/nut-cli/e2e-preview.toml` for the
  preview, then removed. Production `/etc/nut-cli/nut.toml` was not
  installed.
- Binary: `just remote` copies a locally built release `nut` to
  `athena:~/.cache/nut-cli/nut`. That path is user-owned, so this is a
  read-only preview, not the privileged `/usr/local/bin/nut` apply path.

Commands:

```
just remote athena --config ~/.cache/nut-cli/e2e-preview.toml init --dry-run
just remote athena --json --config ~/.cache/nut-cli/e2e-preview.toml init --dry-run
```

A non-root preview is partial when `/etc/nut` is unreadable. It names those
files, intended operator and hook changes, and the required root preview.
It has no apply hash and never prints `Already converged`.

## Recorded output

Captured during phase `tailnet-x.9.1.8` as user `bryan`. Secrets are
absent from this fixture and from the JSON. Live `/etc/nut` contents were
unreadable, so the plan is partial: no apply hash, and it does not print
`Already converged`. `just remote` exits 1 because an incomplete dry-run
is an operational refusal. Root dry-run and apply remain the adopt
phase's responsibility.

Text:

```
 nut init | athena | plan partial
Steps 0 files | 0 services | 0 accounts | 0 other
!! /etc/nut/nut.conf needs root to read
!! /etc/nut/ups.conf needs root to read
!! /etc/nut/upsd.users needs root to read
!! /etc/nut/upsmon.conf needs root to read
!! /etc/nut/upssched.conf needs root to read
Partial preview: required inputs need root to read. No apply hash.
```

JSON:

```
{
  "schema_version": 1,
  "complete": false,
  "hash": null,
  "empty": true,
  "steps": [],
  "explanations": [
    "/etc/nut/nut.conf needs root to read",
    "/etc/nut/ups.conf needs root to read",
    "/etc/nut/upsd.users needs root to read",
    "/etc/nut/upsmon.conf needs root to read",
    "/etc/nut/upssched.conf needs root to read"
  ],
  "secret_intents": [],
  "config_path": "/home/bryan/.cache/nut-cli/e2e-preview.toml",
  "hint": "review the explanations, then re-run with a readable config"
}
```

Operator and hook steps are not listed because those files also need
root. The privileged preview flow is how a later interactive run obtains
an authoritative hash. The staged `e2e-preview.toml` was removed after
these commands.
