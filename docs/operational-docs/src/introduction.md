# Emerald Documentation

## What is Emerald?

Emerald is a **Tendermint-based consensus engine** for Ethereum execution clients, built as a shim layer on top of [Malachite](https://github.com/informalsystems/malachite). It brings **instant finality** to Ethereum-compatible blockchains by connecting Malachite BFT consensus to execution clients through the Engine API.

---

## Key Features

### Instant Finality
Emerald provides **single-slot finality** through Malachite's Tendermint-based BFT consensus. Transactions are finalized immediately once a block is committed.

### Execution Client Agnostic
Emerald integrates with any execution client that supports the **Engine API standard**. While we've implemented and tested with Reth, it should work seamlessly with Geth, Nethermind, or any other Engine API-compliant client.

### Proof of Authority (PoA)
Dynamic validator set management through an on-chain smart contract. Add, remove, or update validators without restarting the network.

### Production Ready Components
Built on battle-tested technology:
- **Malachite**: Formally verified Tendermint consensus implementation
- **Reth**: High-performance Ethereum execution client
- **Engine API**: Standard Ethereum consensus/execution interface

---

## Use Cases

Emerald is designed for scenarios where instant finality and control over the validator set are critical:

### Layer 1 Blockchains
Build custom EVM-compatible Layer 1 chains with:
- Instant transaction finality
- Known and controlled validator sets
- Full EVM compatibility for smart contracts

### Private Networks
Deploy permissioned networks for enterprises or consortiums:
- Controlled validator participation
- Instant finality for financial applications
- Full Ethereum tooling compatibility

### Development and Testing
Rapid iteration for smart contract development:
- Instant block confirmation speeds up testing
- Full control over network parameters
- Compatible with all Ethereum development tools

---

## How It Works

Emerald acts as the **Consensus Layer (CL)** in Ethereum's two-layer architecture, while an execution client like Reth acts as the **Execution Layer (EL)**:

```
┌─────────────────────────────────────────┐
│         Emerald (Consensus Layer)       │
│    ┌──────────────────────────────┐    │
│    │    Malachite BFT Consensus   │    │
│    └──────────────────────────────┘    │
└─────────────────┬───────────────────────┘
                  │ Engine API
┌─────────────────▼───────────────────────┐
│    Reth/Geth (Execution Layer)          │
│    ┌──────────────────────────────┐    │
│    │  EVM, State, Transactions    │    │
│    └──────────────────────────────┘    │
└─────────────────────────────────────────┘
```

**Key Components:**
- **Emerald**: Manages consensus, validator coordination, and block finalization
- **Execution Client**: Handles transaction execution, state management, and EVM operations
- **Engine API**: Standard RPC interface connecting the two layers

For detailed technical architecture, see the [Architecture](./architecture.md) page.

---

## Getting Started

Choose your path based on your needs:

### For Developers & Testing
**[Run a Local Testnet](./local-testnet.md)**

Perfect for:
- Smart contract development
- Testing validator operations
- Learning how Emerald works
- Integration testing

Start a fully functional 3-validator network on your local machine in under a minute.

### For Production Deployments
**[Create a Production Network](./production-network.md)**

Complete guide for:
- Setting up a network with external validators
- Generating and distributing keys
- Configuring nodes for production
- Network coordination and launch

---

## Technical Deep Dive

Want to understand how Emerald works under the hood?

- **[Architecture](./architecture.md)**: Detailed technical architecture, Engine API integration, and consensus flow
- **[Configuration Examples](./config-examples.md)**: Reference configurations and systemd service files

---

## Project Status

Emerald is a **proof of concept** developed by Informal Systems to demonstrate how Malachite can function as a consensus engine for EVM-compatible blockchains through Engine API.

### Current State
- ✅ Working integration with Reth
- ✅ Instant finality consensus
- ✅ PoA validator management
- ✅ Complete documentation
- ⚠️  Under active development

--

## Links

- **[Emerald GitHub Repository](https://github.com/informalsystems/emerald)**
- **[Malachite BFT](https://github.com/informalsystems/malachite)** - The underlying consensus engine
- **[Reth](https://github.com/paradigmxyz/reth)** - Ethereum execution client

--

## Contributing & Support

Emerald is developed by [Informal Systems](https://informal.systems). For questions, issues, or contributions, please visit the GitHub repository.
