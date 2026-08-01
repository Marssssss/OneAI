# OneAI DomainPack Mechanism

> Pull "domain knowledge" out of hardcoded logic into a declarative, mergeable, validatable, one-line-switchable config pack.

## Responsibility

DomainPack lets the same engine switch seamlessly across domains (coding / research / general) without code changes. A pack bundles 7 layers of domain config; packs can merge (multi-domain agents), validate against a JSON Schema, install from path or git, and share via a market.

## The 7 layers

| Layer | Component | Purpose |
|---|---|---|
| 1 | Tools + ToolDecorator | Domain-specific tool set + description overrides |
| 2 | ContextSource | Domain-specific environment sensing (with refresh policy) |
| 3 | PermissionProfile | Domain permission classification (deny / auto / confirm) |
| 4 | ParadigmStrategy | Domain task→paradigm mapping |
| 5 | CompressionTemplate | Domain context-retention priority |
| 6 | Workflow + StateGraph | Domain-predefined workflows & cyclic graphs |
| 7 | MemoryProfile | Domain memory policy (extraction schema / recall / core budget / self-managed tools / cross-session habits) |

`MemoryProfile` also carries the Working-State policy and memory-decay policy (see [Memory mechanism](memory-mechanism_EN.md) and [Working-State mechanism](working-state-mechanism_EN.md)).

## Key types & files

| Item | Location |
|---|---|
| `DomainPack` 7-layer definition | `crates/oneai-domain/src/domain_pack.rs` |
| `CodingPack` reference impl | `crates/oneai-domain/src/coding_pack.rs` |
| `ContainerizedCodingPack` (VM/container as boundary) | `crates/oneai-domain/src/containerized_pack.rs` |
| Pack market (`PackSource`/`PackRegistry`) | `crates/oneai-domain/src/market.rs` |
| `DomainPackSpec` + validator | `crates/oneai-domain/src/config_parser.rs` |
| `ContextSource` + refresh policy | `crates/oneai-domain/src/context_source.rs` |
| Compression template | `crates/oneai-domain/src/compression_template.rs` |

## Core flow

```rust
let app = AppBuilder::new()
    .provider(provider)
    .domain_pack(coding_pack("/project/dir"))  // ← one-line domain switch
    .build()?;
```

Packs merge for multi-domain agents (coding + research): permissions are "strictest-wins", context sources merge by priority. A pack can be validated structurally + semantically against `DomainPackSpec` (JSON Schema), and `pack install`ed from local path or git.

## Related CLI

[`pack list / show / install / validate / spec / check`](cli-reference_EN.md#domainpack-domain-config-pack).

## Further reading

- [CLAUDE.md — DomainPack](../CLAUDE.md) (7-layer definition, merge rules, Footprint ladder)
- Reference impl: [CodingPack](../crates/oneai-domain/src/coding_pack.rs)
