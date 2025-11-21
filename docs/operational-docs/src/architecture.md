# Architecture

## Overview

Emerald is a consensus engine that brings Malachite BFT (Tendermint-based consensus) to Ethereum execution clients through the Engine API. This page explains the technical architecture and how the components work together.

---

## System Architecture

Ethereum's architecture consists of two primary layers: Consensus Layer (CL) and Execution Layer (EL), with Engine API serving as the bridge between both. Emerald functions as the consensus engine (CL) for Ethereum execution clients (EL) through Engine API.

By leveraging Malachite's channel-based interface, we built a lightweight shim layer on top that integrates seamlessly with any execution client supporting Engine API. For our implementation we have chosen Reth as the execution client, but the design is agnostic and should work with any Engine API-compliant client, such as Geth or Nethermind.

![Emerald Architecture](images/malaketh-layered-0.png)

### Key Architectural Components

- **Emerald Application**: The shim layer that connects Malachite to the execution client
- **Malachite Consensus**: BFT consensus engine providing instant finality
- **Engine API**: Standardized RPC interface for consensus/execution communication
- **Execution Client (Reth)**: Handles transaction execution, state management, and EVM operations

---

## Engine API

Engine API plays a central role in Ethereum's post-merge architecture, defining a standardized RPC interface between the Consensus Layer (CL) and Execution Layer (EL).

### Layer Responsibilities

**Consensus Layer (CL):**
- Agreeing on the canonical chain
- Finalizing blocks
- Managing validator set
- Coordinating consensus protocol

**Execution Layer (EL):**
- Block creation and processing
- Transaction execution
- State management
- Blockchain storage
- Mempool management
- RPC interfaces for applications

### Engine API Methods

From the perspective of Engine API, the CL is a client that makes RPC calls with Engine API methods to the EL, the RPC server. Key methods are:

- **`forkchoiceUpdated`**: Updates the execution client with the latest chain head and final block. If called with a `PayloadAttributes` parameter, it instructs the client to build a new block. This method also plays a role in Ethereum's finality mechanism by marking blocks as finalized.

- **`getPayload`**: Retrieves a newly constructed block from the execution client after calling `forkchoiceUpdated` with `PayloadAttributes`.

- **`newPayload`**: Submits a proposed block to the execution client for validation and inclusion in the chain. Note that it does not change the tip of the chain, which is the job of `forkchoiceUpdated`.

---

## Malachite as a Library

Malachite offers three interfaces at different abstraction levels: Low-level, Actors, and Channels. These interfaces range from fine-grained control to ready-to-use functionality.

### Channel-Based Interface

In Emerald, we use the Channel-based interface, which prioritizes ease of use over customization. It provides built-in synchronization, crash recovery, networking for consensus voting, and block propagation protocols.

Application developers only need to interact with Malachite through a channel that emits events, such as:

- **`AppMsg::ConsensusReady { reply }`**: Signals that Malachite is initialized and ready to begin consensus.

- **`AppMsg::GetValue { height, round, timeout, reply }`**: Requests a value (e.g., a block) from the application when the node is the proposer for a given height and round.

- **`AppMsg::ReceivedProposalPart { from, part, reply }`**: Delivers parts of a proposed value from other nodes, which are reassembled into a complete block.

- **`AppMsg::Decided { certificate, reply }`**: Notifies the application that consensus has been reached, providing a certificate with the decided value and supporting votes.

Malachite sends additional messages (e.g., for synchronization), but we focus only on the core events relevant to this integration. Each event includes a `reply` callback, allowing the application to respond to Malachite.

---

## Connecting Malachite to Engine API

Emerald is an application built on top of Malachite, which is unaware of Engine API and only exposes the Channels interface.

### Application Components

The application includes two main components for interacting with the execution client:

1. **RPC Client**: With JWT authentication to send Engine API requests to the execution client.

2. **Internal State**: Keeps track of values such as the latest block and the current height, round, and proposer. It also maintains persistent storage for proposals and block data to support block propagation.

Our integration revolves around three scenarios: initializing consensus, proposing a block as the proposer, and voting as a non-proposer. Below we outline how Malachite's events map to Engine API calls.

### Consensus Initialization

When Malachite starts, it sends a `AppMsg::ConsensusReady` event to signal the app that is ready. For simplicity, we assume all nodes begin from a clean state (height one) without needing to sync with an existing network. Each execution client initializes from the same genesis file, producing an initial block (block number 1) with a `parent_hash` of `0x0`.

<img src="images/malaketh-layered-1.png" width="800" />

Emerald queries the execution client via the `eth_getBlockByNumber` RPC endpoint to fetch the latest committed block (in this case, the genesis block). This block is stored in the application state and serves as the base for building subsequent blocks.

### Proposing and Committing a Block

When a node becomes the proposer for a given height and round, the application receives from Malachite a `AppMsg::GetValue` event. The node must propose a new block to the network. Here's how the application drives this process:

1. The application calls `forkchoiceUpdated` with `PayloadAttributes` to instruct the execution client to build a new block. If the parameters are valid and everything goes as expected, the RPC method will return a `payload_id`.

2. Immediately, it calls `getPayload` with the `payload_id` of the previous step to retrieve an execution payload (the block).

3. The block is stored in the app's local state and is sent back to Malachite via the `reply` callback, where it's propagated to other validators.

At this moment validators exchange Tendermint votes to reach consensus. Once agreed, Malachite emits `AppMsg::Decided` to the application, which finalizes the block in the execution client with the following steps:

1. Retrieve the stored block and compute its hash.
2. Call `forkchoiceUpdated` with the block's hash (no `PayloadAttributes`) to set the block as the head of the chain and finalize it.
3. Update the local state with the new block and certificate. Finally, signal Malachite to proceed to the next height.

<img src="images/malaketh-layered-2.png" width="800" />

### Voting and Committing as a Non-Proposer

As a non-proposer, the application receives `AppMsg::ReceivedProposalPart` events with block fragments. Once all parts are re-assembled, the block is stored locally. Eventually, Malachite concludes consensus by emitting a `AppMsg::Decided` event. The application then calls `newPayload` to submit the decided block to the execution client, followed by `forkchoiceUpdated` to update the chain head and finalize the block.

<img src="images/malaketh-layered-3.png" width="800" />
