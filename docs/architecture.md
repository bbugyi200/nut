# Architecture

`nut` is a management plane beside Network UPS Tools. NUT's daemons, `upssched`, and a
tiny rendered shell hook own the outage path. Nothing in that path invokes `nut`. If the
binary is missing or broken, the host still shuts down cleanly.

One Cargo package, `nut-cli`, with binary `nut` and library `nut_cli`. It is synchronous
Rust with no async runtime. The library declares `#![forbid(unsafe_code)]`.

Host policy versus hardware knowledge: UPS and board profiles are generic data in this
repo. Machine choices (timers, mounts, mail, operators, battery date) live in a host
config that the machine's configuration repository owns. This repo never contains a
real mailbox or athena-only secrets. Golden fixtures are redacted.

## Figure 1. Which parts of an outage response does nut touch?

```mermaid
---
title: "Which parts of athena's outage response does nut touch?"
accTitle: "nut is a management plane beside NUT's protection path"
accDescr: "The protection path runs from the UPS through the NUT driver and upsd, then upsmon, upssched and a shell hook, then a clean shutdown with an outlet cycle, then the BIOS Power On setting. The nut CLI only writes configuration, reads status, and guides and records BIOS and drill evidence; it is never called during an outage."
config:
  layout: dagre
  flowchart:
    rankSpacing: 34
    nodeSpacing: 14
    wrappingWidth: 260
    subGraphTitleMargin:
      top: 6
      bottom: 10
---
flowchart TB
  subgraph MGMT["Management plane: the nut CLI"]
    direction LR
    INIT["`**nut init**<br/>writes config`"]
    READ["`**doctor, status**<br/>read only`"]
    GUIDE["`**bios, drill**<br/>guide and attest`"]
  end
  subgraph DATA["Protection path: works with nut uninstalled"]
    direction TB
    UPSD["`**UPS to upsd**<br/>USB, usbhid-ups driver`"]
    MON["`**upsmon, upssched, hook**<br/>30 s email, 180 s shutdown`"]
    FSD["`**Clean shutdown**<br/>then UPS outlet off/on`"]
    BIOS["`**BIOS Power On**<br/>boots when outlets return`"]
    UPSD --> MON --> FSD --> BIOS
  end
  INIT -.->|"renders"| MON
  READ -.->|"reads"| UPSD
  GUIDE -.->|"guides, attests"| BIOS
  classDef focus fill:#E8F1FB,stroke:#3B78C2,stroke-width:1.5px,color:#0F2E52
  classDef outcome fill:#E7F5EC,stroke:#2E7D4F,stroke-width:1.5px,color:#123D24
  class UPSD,MON,FSD,BIOS focus
  class INIT,READ,GUIDE outcome
  style DATA fill:none,stroke:#3B78C2
  style MGMT fill:none,stroke:#2E7D4F
```

_Figure 1. Everything in the blue box runs during an outage, and none of it calls `nut`.
The green box writes files ahead of time, reads state, and records evidence. A broken
`nut` build can make the terminal ugly; it cannot stop a clean shutdown._

## Modules

- `cli/`: the clap derive tree, the grouped help renderer, and dispatch.
- `ui/`: theme, glyphs, formatting, bars, layout, and tables.
- `host/`: a `Host` trait for filesystem, process, clock, env, euid, TTY, and hostname.
- `upsd/`: the RFC 9271 client and the loopback-only fake upsd.
- `config/`, `profiles/`, `render/`, `init/`, `doctor/`, `firmware/`, `drill/`,
  `actions/`, `completion/`, `journal/`.

Machine paths: `/usr/local/bin/nut` (root-owned), `/etc/nut-cli/nut.toml`,
`/etc/nut-cli/operator.secret`, `/var/lib/nut-cli/`, `/var/backups/nut-cli/<UTC stamp>/`,
the hook `/usr/local/sbin/nut-upssched-cmd`, and per-user completion state under
`~/.local/state/nut-cli/`.

## Related

- [Design language](design.md)
- [Safety contract](safety.md)
- [Outage timeline](outage-timeline.md)
- [Init and rollback](init.md)
- [Development](development.md)
