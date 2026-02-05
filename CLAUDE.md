# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

saorsa-gossip is the gossip protocol library for the Saorsa ecosystem. It provides the CRDT-based synchronization and message routing layer used by all Saorsa applications including saorsa-node and communitas.

## 🤖 MANDATORY SUBAGENT USAGE

**ALWAYS USE SUBAGENTS. THIS IS NOT OPTIONAL.**

Subagents (via the `Task` tool) MUST be used whenever possible. They provide:
- **Fresh context** - Each subagent starts clean, preventing context pollution
- **Parallel execution** - Multiple agents can work simultaneously
- **Specialization** - Purpose-built agents for specific tasks
- **Unbounded execution** - No context limits when chaining agents

### When to Spawn Subagents

| Task Type | Action |
|-----------|--------|
| Code exploration/search | Spawn `Explore` agent |
| Code review | Spawn review agents in parallel |
| Bug fixes | Spawn `code-fixer` agent |
| Test execution | Spawn `test-runner` agent |
| Build validation | Spawn `build-validator` agent |
| Security scanning | Spawn `security-scanner` agent |
| Documentation audit | Spawn `documentation-auditor` agent |
| Multi-step tasks | Spawn `dev-agent` or `general-purpose` agent |
| Complex research | Spawn `Explore` or `general-purpose` agent |

### Subagent Rules

1. **PREFER subagents over doing work directly** - Even for "simple" tasks
2. **PARALLELIZE when possible** - Spawn multiple agents in a single message
3. **Use background agents** for long-running tasks (`run_in_background: true`)
4. **Chain agents** for complex workflows - Output of one feeds the next
5. **Never accumulate context** - Delegate to fresh agents instead

**IF YOU CAN USE A SUBAGENT, YOU MUST USE A SUBAGENT.**

## Development Commands

### Building and Testing
```bash
# Build the project
cargo build --release

# Run all tests
cargo nextest run

# Run with verbose output
cargo nextest run --no-capture

# Format and lint
cargo fmt --all
cargo clippy --all-features -- -D clippy::panic -D clippy::unwrap_used -D clippy::expect_used

# Run benchmarks
cargo bench
```

### Running Examples
```bash
# Run gossip simulation
cargo run --example gossip_simulation

# Run with debug logging
RUST_LOG=debug cargo run --example gossip_simulation
```

## Code Standards

### NO PANICS IN PRODUCTION CODE
- No `.unwrap()` - Use `?` operator or `.ok_or()`
- No `.expect()` - Use `.context()` from `anyhow`
- No `panic!()` - Return `Result` instead
- **Exception**: Test code may use these for assertions

---

## 🚨 CRITICAL: Saorsa Network Infrastructure & Port Isolation

### Infrastructure Documentation
Full infrastructure documentation is available at: `docs/infrastructure/INFRASTRUCTURE.md`

This includes:
- All 9 VPS nodes across 3 cloud providers (DigitalOcean, Hetzner, Vultr)
- Bootstrap node endpoints and IP addresses
- Firewall configurations and SSH access
- Systemd service templates

### ⚠️ PORT ALLOCATION

saorsa-gossip is a library used by multiple applications. Each application uses a dedicated port RANGE:

| Service | UDP Port Range | Default | Description |
|---------|----------------|---------|-------------|
| ant-quic | 9000-9999 | 9000 | QUIC transport layer |
| saorsa-node | 10000-10999 | 10000 | Core P2P network nodes |
| communitas | 11000-11999 | 11000 | Collaboration platform nodes |

### 🛑 DO NOT DISTURB OTHER NETWORKS

When testing saorsa-gossip functionality:

1. **Be aware** of which application you're testing with
2. **Use ports within that application's range**
3. **NEVER** kill processes on ports used by other applications
4. Each network may be running independent tests - respect port boundaries

### Testing Guidelines

When writing integration tests that connect to VPS nodes:
- **ant-quic tests**: Use ports 9000-9999 only
- **saorsa-node tests**: Use ports 10000-10999 only
- **communitas tests**: Use ports 11000-11999 only

```bash
# ✅ CORRECT - Testing gossip with specific application
# For saorsa-node integration (use 10000-10999)
cargo nextest run --features saorsa-node --test-threads=1

# ❌ WRONG - Would disrupt other networks
ssh root@saorsa-2.saorsalabs.com "pkill -f gossip"  # Too broad - affects all services
ssh root@saorsa-2.saorsalabs.com "pkill -f ':9'"    # NEVER - matches ant-quic ports
```

### Bootstrap Endpoints (by application)
```
# ant-quic (port range 9000-9999, default 9000)
saorsa-2.saorsalabs.com:9000
saorsa-3.saorsalabs.com:9000

# saorsa-node (port range 10000-10999, default 10000)
saorsa-2.saorsalabs.com:10000
saorsa-3.saorsalabs.com:10000

# communitas (port range 11000-11999, default 11000)
saorsa-2.saorsalabs.com:11000
saorsa-3.saorsalabs.com:11000
```

### Before Any VPS Operations
1. Identify which application layer you're testing
2. Use only ports within that application's designated range
3. Never run broad `pkill` commands that could affect other services
4. Coordinate with other teams if testing across multiple port ranges
