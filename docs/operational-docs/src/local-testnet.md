# **Testnet Setup Guide**

This guide explains how to create and manage a local Emerald testnet using the Makefile and the Proof-of-Authority (PoA) utilities.

---

## **Table of Contents**

- [Overview](#overview)
- [Creating a New Network](#creating-a-new-network)
- [Managing Validators with PoA Tools](#managing-validators-with-poa-tools)
- [Network Configuration](#network-configuration)
- [Interacting with the Network](#interacting-with-the-network)
- [Network Operations](#network-operations)
- [Monitoring](#monitoring)
- [Common Development Workflows](#common-development-workflows)
- [Troubleshooting](#troubleshooting)

---

## **Overview**

This guide is for **developers and testers** who want to run a local Emerald testnet on their machine for development, testing, and experimentation.

### **What is a Local Testnet?**

A local testnet is a fully functional blockchain network running entirely on your computer. It provides:

- **Fast iteration**: Test smart contracts and applications without waiting for public networks
- **Complete control**: Add/remove validators, modify network parameters, reset state anytime
- **No cost**: No real tokens required for testing
- **Privacy**: All transactions and data stay on your machine
- **Instant finality**: Malachite BFT consensus provides immediate block finalization

### **Use Cases**

- Developing and testing smart contracts
- Testing validator operations and network behavior
- Experimenting with PoA validator management
- Integration testing for dApps
- Learning how Emerald consensus works

### **Architecture**

Emerald uses Malachite BFT consensus connected to Reth execution clients via Engine API. The testnet setup creates multiple validator nodes that reach consensus on blocks with instant finality.

- **Consensus Layer**: Malachite BFT (instant finality)
- **Execution Layer**: Reth (Ethereum execution client)
- **Connection**: Engine API with JWT authentication
- **Validator Management**: ValidatorManager PoA smart contract at `0x0000000000000000000000000000000000002000`

### **Difference from Production Networks**

| Feature | Local Testnet | Production Network |
|---------|---------------|-------------------|
| Validators | All on your machine | Distributed across organizations |
| Data persistence | Can reset anytime | Permanent blockchain history |
| Network access | Localhost only | Public or permissioned network |
| Use case | Development/testing | Real applications |
| Setup time | ~30 seconds | Requires coordination |

---

## **Creating a New Network (devnet)**

### **Prerequisites**

Before starting, ensure you have:

- **Rust**: Install from https://rust-lang.org/tools/install/
- **Docker**: Install from https://docs.docker.com/get-docker/
- **Docker Compose**: Usually included with Docker Desktop
- **Make**: Typically pre-installed on Linux/macOS; Windows users can use WSL
- **Git**: For cloning the repository

**Verify installations:**
```bash
rust --version   # Should show rustc 1.70+
docker --version # Should show Docker 20.10+
make --version   # Should show GNU Make
```

### **Quick Start: 3-Validator Network**

The default configuration creates a 3-validator network. From the repository root, run:

```bash
make
```

This single command performs all setup automatically:

1. **Cleans previous testnet data** - Removes any old network state
2. **Builds the project** - Compiles Solidity contracts and Rust binaries
3. **Generates testnet configuration** - Creates network parameters for 3 nodes
4. **Creates validator keys** - Generates private keys for each validator
5. **Creates node directories** - Sets up `nodes/0/`, `nodes/1/`, `nodes/2/`
6. **Extracts validator public keys** - Collects pubkeys into `nodes/validator_public_keys.txt`
7. **Generates genesis file** - Creates `assets/genesis.json` with initial validator set
8. **Starts Docker containers** - Launches Reth nodes, Prometheus, Grafana, Otterscan
9. **Configures peer connections** - Connects all Reth nodes to each other
10. **Spawns Emerald consensus nodes** - Starts the consensus layer for each validator

**Expected output:** You should see logs from all 3 validators producing blocks. The network should start producing blocks within a few seconds.

### **Verify Network is Running**

Once the command completes, verify the network is operational:

```bash
# Check if blocks are being produced
curl -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'

# Expected output: {"jsonrpc":"2.0","id":1,"result":"0x5"} (or higher block number)
```

**Access monitoring tools:**
- **Grafana Dashboard**: http://localhost:3000 (metrics visualization)
- **Prometheus**: http://localhost:9090 (raw metrics data)
- **Otterscan Block Explorer**: http://localhost:5100 (view blocks and transactions)

### **4-Validator Network**

To create a network with 4 validators:

```bash
make four
```

### **What Happens During Network Creation**

1. **Configuration Generation** (`./scripts/generate_testnet_config.sh`)
   - Creates `.testnet/testnet_config.toml` with network parameters

2. **Validator Key Generation**

   ```bash
   cargo run --bin emerald -- testnet \
     --home nodes \
     --testnet-config .testnet/testnet_config.toml
   ```

   - Creates `nodes/0/`, `nodes/1/`, etc.
   - Each node gets a `config/priv_validator_key.json`

3. **Public Key Extraction**

   ```bash
   cargo run --bin emerald show-pubkey \
     nodes/0/config/priv_validator_key.json
   ```

   - Outputs public keys to `nodes/validator_public_keys.txt`

4. **Genesis File Generation**

   ```bash
   cargo run --bin emerald-utils genesis \
     --public-keys-file ./nodes/validator_public_keys.txt
   ```

   - Creates `assets/genesis.json` with:
     - Initial validator set (3 validators with power 100 each)
     - ValidatorManager contract deployed at genesis
     - Ethereum genesis block configuration

5. **Network Startup**
   - Docker Compose starts Reth execution clients
   - Each Reth node initializes from `assets/genesis.json`
   - Peer connections established
   - Emerald consensus nodes spawn and connect to Reth via Engine API

---

## **Managing Validators with PoA Tools**

Once the network is running, you can dynamically manage validators using the Rust-based PoA utilities. This allows you to add, remove, or update validators without restarting the network.

### **What is Proof of Authority (PoA)?**

The Emerald network uses a PoA smart contract (`ValidatorManager`) to manage the validator set. This contract is deployed at a predefined address (`0x0000000000000000000000000000000000002000`) and controls:

- Which validators are active
- Each validator's voting power
- Who can modify the validator set (the contract owner)

**Use cases for PoA tools:**
- **Testing validator changes**: Simulate adding/removing validators in a running network
- **Testing voting power**: Experiment with different power distributions
- **Integration testing**: Test how your application handles validator set changes
- **Learning**: Understand how dynamic validator management works

### **Prerequisites**

Before using PoA tools, ensure:

- **Network is running**: Start with `make` or `make four`
- **RPC endpoint accessible**: Default is `http://127.0.0.1:8545`
- **Contract owner key available**: See below for default test key

### **Understanding the Test Accounts**

The local testnet uses a well-known test mnemonic for pre-funded accounts:

**Mnemonic**: `test test test test test test test test test test test junk`

**PoA Contract Owner (Account #0)**:
- **Private Key**: `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80`
- **Address**: `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266`
- **Role**: Has authority to add/remove/update validators

**Validator Keys**:
- Located at `nodes/{0,1,2}/config/priv_validator_key.json`
- These are separate from the Ethereum accounts
- Used for consensus signing, not transactions

**Important**: These keys are for **testing only**. Never use them on public networks or with real funds.

### **List Current Validators**

View all registered validators and their voting power:

```bash
cargo run --bin emerald-utils poa list
```

**Output:**

```
Total validators: 3

Validator #1:
  Power: 100
  Pubkey: 04681eaaa34e491e6c8335abc9ea92b024ef52eb91442ca3b84598c79a79f31b75...
  Validator address: 0x1234567890abcdef...

Validator #2:
  Power: 100
  ...
```

### **Add a New Validator**

To add a new validator to the active set:

First get the pubkey of the validator you want to add by running:

```bash
cargo run --bin emerald show-pubkey \
  path/to/new/validator/priv_validator_key.json
```

Then run the following command, replacing the placeholder values:

```bash
cargo run --bin emerald-utils poa add-validator \
  --validator-pubkey 0x04abcdef1234567890... \
  --power 100 \
  --owner-private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

**Parameters:**

- `--validator-pubkey`: Uncompressed secp256k1 public key (65 bytes with `0x04` prefix, or 64 bytes raw)
- `--power`: Voting weight (default: 100)
- `--owner-private-key`: Private key of the ValidatorManager contract owner

**Optional flags:**

- `--rpc-url`: RPC endpoint (default: `http://127.0.0.1:8545`)
- `--contract-address`: ValidatorManager address (default: `0x0000000000000000000000000000000000002000`)

### **Remove a Validator**

To remove a validator from the active set:

```bash
cargo run --bin emerald-utils poa remove-validator \
  --validator-pubkey 0x04681eaaa34e491e6c8335abc9ea92b024ef52eb91442ca3b84598c79a79f31b75... \
  --owner-private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

### **Update Validator Power**

To change a validator's voting weight:

```bash
cargo run --bin emerald-utils poa update-validator \
  --validator-pubkey 0x04681eaaa34e491e6c8335abc9ea92b024ef52eb91442ca3b84598c79a79f31b75... \
  --power 200 \
  --owner-private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

## **Network Configuration**

### **Default Addresses**

- **ValidatorManager Contract**: `0x0000000000000000000000000000000000002000`
- **RPC Endpoints**:
  - Node 0: `http://127.0.0.1:8545` (primary endpoint for most operations)
  - Node 1: `http://127.0.0.1:8546`
  - Node 2: `http://127.0.0.1:8547`
  - Node 3 (if running): `http://127.0.0.1:8548`

**Note**: All nodes share the same blockchain state. You can connect to any endpoint, but `8545` is typically used as the default.

### **Genesis Validators**

The genesis file is generated with 3 initial validators, each with power 100. Validator public keys are extracted from:

- `nodes/0/config/priv_validator_key.json`
- `nodes/1/config/priv_validator_key.json`
- `nodes/2/config/priv_validator_key.json`

### **Pre-funded Test Accounts**

The genesis file pre-funds accounts from the test mnemonic with ETH for testing:

**Mnemonic**: `test test test test test test test test test test test junk`

| Account # | Address | Private Key | Initial Balance |
|-----------|---------|-------------|-----------------|
| 0 | `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` | `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80` | 10,000 ETH |
| 1 | `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` | `0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d` | 10,000 ETH |
| 2 | `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` | `0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a` | 10,000 ETH |

Use these accounts for sending transactions, deploying contracts, or testing.

---

## **Interacting with the Network**

Once your local testnet is running, you can interact with it like any Ethereum network.

### **Using curl (JSON-RPC)**

**Get current block number:**
```bash
curl -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

**Get account balance:**
```bash
curl -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_getBalance","params":["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","latest"],"id":1}'
```

**Send a transaction:**
```bash
curl -X POST http://127.0.0.1:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc":"2.0",
    "method":"eth_sendTransaction",
    "params":[{
      "from":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
      "to":"0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
      "value":"0x1000000000000000000",
      "gas":"0x5208"
    }],
    "id":1
  }'
```

### **Using cast (Foundry)**

If you have [Foundry](https://book.getfoundry.sh/getting-started/installation) installed:

**Get block number:**
```bash
cast block-number --rpc-url http://127.0.0.1:8545
```

**Check balance:**
```bash
cast balance 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://127.0.0.1:8545
```

**Send ETH:**
```bash
cast send 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 \
  --value 1ether \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --rpc-url http://127.0.0.1:8545
```

### **Using Web3 Libraries**

Configure your Web3 library to connect to `http://127.0.0.1:8545`:

**ethers.js (JavaScript):**
```javascript
const { ethers } = require('ethers');

const provider = new ethers.JsonRpcProvider('http://127.0.0.1:8545');
const wallet = new ethers.Wallet('0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80', provider);

// Send transaction
const tx = await wallet.sendTransaction({
  to: '0x70997970C51812dc3A010C7d01b50e0d17dc79C8',
  value: ethers.parseEther('1.0')
});
await tx.wait();
```

**web3.py (Python):**
```python
from web3 import Web3

w3 = Web3(Web3.HTTPProvider('http://127.0.0.1:8545'))
account = w3.eth.account.from_key('0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80')

# Send transaction
tx = {
    'to': '0x70997970C51812dc3A010C7d01b50e0d17dc79C8',
    'value': w3.to_wei(1, 'ether'),
    'gas': 21000,
    'gasPrice': w3.eth.gas_price,
    'nonce': w3.eth.get_transaction_count(account.address),
}
signed_tx = account.sign_transaction(tx)
tx_hash = w3.eth.send_raw_transaction(signed_tx.rawTransaction)
```

### **Using MetaMask**

To connect MetaMask to your local testnet:

1. Open MetaMask and click on the network dropdown
2. Click "Add Network" → "Add a network manually"
3. Enter the following details:
   - **Network Name**: Emerald Local
   - **RPC URL**: `http://127.0.0.1:8545`
   - **Chain ID**: `12345` (or whatever you set in genesis)
   - **Currency Symbol**: ETH
4. Click "Save"
5. Import one of the test accounts using its private key

**Security Note**: Only use test private keys with local networks. Never import test keys into wallets used for real funds.

---

## **Network Operations**

### **Stop the Network**

```bash
make stop
```

This stops all Docker containers but preserves data.

### **Clean the Network**

```bash
make clean
```

**Warning**: This deletes:

- All node data (`nodes/`)
- Genesis file (`assets/genesis.json`)
- Testnet config (`.testnet/`)
- Docker volumes (Reth databases)
- Prometheus/Grafana data

### **Restart a Clean Network**

```bash
make clean
make
```

---

## **Troubleshooting**

### **Network Won't Start**

1. **Check if ports are in use**:

   ```bash
   lsof -i :8545  # RPC port
   lsof -i :30303 # P2P port
   ```

2. **View Docker logs**:

   ```bash
   docker compose logs reth0
   docker compose logs reth1
   ```

3. **Verify genesis file exists**:

   ```bash
   ls -la assets/genesis.json
   ```

4. **Check emerald logs**:
   ```bash
   tail -f nodes/0/emerald.log
   ```

### **Validator Operations Fail**

1. **Verify network is running**:

   ```bash
   curl -X POST http://127.0.0.1:8545 \
     -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
   ```

2. **Check validator public key format**:
   - Must be hex-encoded secp256k1 public key
   - Can be 64 bytes (raw) or 65 bytes (with `0x04` prefix)
   - Include `0x` prefix

3. **Verify contract owner key**:
   - Default: `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80`

### **Public Key Extraction**

To get a validator's public key from their private key file:

```bash
cargo run --bin emerald show-pubkey \
  nodes/0/config/priv_validator_key.json
```

---

## **Monitoring**

The `make` command automatically starts monitoring services to help you observe network behavior.

### **Grafana - Metrics Visualization**

**URL**: http://localhost:3000

Grafana provides visual dashboards for monitoring validator and network metrics.

**Default credentials:**
- Username: `admin`
- Password: `admin` (you'll be prompted to change this on first login, but you can skip it for local testing)

**What to monitor:**
- **Block production rate**: Are validators producing blocks consistently?
- **Consensus metrics**: Round times, vote counts, proposal statistics
- **Node health**: CPU, memory, disk usage
- **Network metrics**: Peer connections, message rates

**Tip**: If you don't see data immediately, wait 30-60 seconds for metrics to accumulate.

### **Prometheus - Raw Metrics**

**URL**: http://localhost:9090

Prometheus collects time-series metrics from all nodes. Use the query interface to explore raw metrics data.

**Useful queries:**
- `emerald_consensus_height` - Current consensus height per node
- `emerald_consensus_round` - Current consensus round
- `emerald_mempool_size` - Number of transactions in mempool
- `process_cpu_seconds_total` - CPU usage per process

**When to use Prometheus:**
- Creating custom queries
- Debugging specific metric issues
- Exporting data for analysis

### **Otterscan - Block Explorer**

**URL**: http://localhost:5100

Otterscan is a lightweight block explorer for inspecting blocks, transactions, and accounts.

**Features:**
- View recent blocks and transactions
- Search by address, transaction hash, or block number
- Inspect contract interactions
- View account balances and transaction history

**Use cases:**
- Verify transactions were included in blocks
- Debug smart contract interactions
- Inspect validator activity
- View network state

### **Emerald Node Logs**

View consensus logs for each validator:

```bash
# View logs from validator 0
tail -f nodes/0/emerald.log

# View logs from all validators simultaneously
tail -f nodes/{0,1,2}/emerald.log
```

**What to look for:**
- Block proposals and commits
- Consensus round progression
- Validator voting activity
- Any errors or warnings

### **Docker Container Logs**

View Reth execution client logs:

```bash
# View logs from Reth node 0
docker compose logs -f reth0

# View all Reth logs
docker compose logs -f reth0 reth1 reth2
```

**What to look for:**
- Block execution confirmations
- Transaction processing
- Peer connection status
- Engine API communication with Emerald

---

## **Common Development Workflows**

Here are some typical workflows for using the local testnet during development.

### **Workflow 1: Testing Smart Contract Deployment**

1. **Start the network:**
   ```bash
   make
   ```

2. **Deploy your contract using Foundry:**
   ```bash
   forge create src/MyContract.sol:MyContract \
     --rpc-url http://127.0.0.1:8545 \
     --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
   ```

3. **Verify in Otterscan:**
   - Open http://localhost:5100
   - Search for the contract address
   - View deployment transaction and contract state

4. **Interact with the contract:**
   ```bash
   cast call <CONTRACT_ADDRESS> "myFunction()" --rpc-url http://127.0.0.1:8545
   ```

### **Workflow 2: Testing Validator Set Changes**

1. **Start the network:**
   ```bash
   make
   ```

2. **Check initial validator set:**
   ```bash
   cargo run --bin emerald-utils poa list
   ```

3. **Create a new validator key:**
   ```bash
   cargo run --bin emerald -- init --home nodes/new_validator
   ```

4. **Get the public key:**
   ```bash
   cargo run --bin emerald show-pubkey nodes/new_validator/config/priv_validator_key.json
   ```

5. **Add the validator to the network:**
   ```bash
   cargo run --bin emerald-utils poa add-validator \
     --validator-pubkey <PUBKEY> \
     --power 100 \
     --owner-private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
   ```

6. **Verify the change:**
   ```bash
   cargo run --bin emerald-utils poa list
   ```

7. **Start the new validator node** (manual process, see node configuration)

### **Workflow 3: Testing Under Load**

1. **Start the network:**
   ```bash
   make
   ```

2. **Run the transaction spammer** (if available in your repo):
   ```bash
   cargo run --bin tx-spammer -- \
     --rpc-url http://127.0.0.1:8545 \
     --rate 10 \
     --duration 60
   ```

3. **Monitor performance in Grafana:**
   - Open http://localhost:3000
   - Watch block production rate
   - Monitor transaction processing time
   - Check for any consensus delays

4. **Check mempool and logs:**
   ```bash
   # Check mempool size
   curl -X POST http://127.0.0.1:8545 \
     -H "Content-Type: application/json" \
     -d '{"jsonrpc":"2.0","method":"txpool_status","params":[],"id":1}'

   # Watch validator logs
   tail -f nodes/0/emerald.log
   ```

### **Workflow 4: Iterative Development (Reset & Restart)**

When you need a clean state:

1. **Stop and clean:**
   ```bash
   make clean
   ```

2. **Restart fresh:**
   ```bash
   make
   ```

3. **Redeploy contracts and test again**

**Tip**: This is faster than manually resetting blockchain state and ensures a consistent starting point.

### **Workflow 5: Testing Application Integration**

1. **Start the network:**
   ```bash
   make
   ```

2. **Configure your application** to use:
   - RPC URL: `http://127.0.0.1:8545`
   - Chain ID: `12345`
   - Test account private key (from pre-funded accounts)

3. **Run your application** and verify:
   - Transactions are submitted successfully
   - Events are emitted and captured correctly
   - State changes are reflected

4. **Use Otterscan** to debug any issues:
   - View transaction details
   - Check revert reasons
   - Inspect logs and events

---

## **References**

- [Main README](../../README.md) - Project overview and architecture
- [Makefile](../../Makefile) - Build and deployment automation
- [ValidatorManager.sol](../../solidity/src/ValidatorManager.sol) - Validator registry contract
- [utils/src/poa.rs](../../utils/src/poa.rs) - PoA management utilities
