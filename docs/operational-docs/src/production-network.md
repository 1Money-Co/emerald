# **Starting a New Network**

### **Overview**

This guide is designed for **network coordinators** (companies, foundations, or organizations) who want to launch a new blockchain network using Emerald consensus and Reth execution clients.

#### **Who is this guide for?**

You are the **network coordinator** responsible for:
- Recruiting and onboarding external validators to participate in your network
- Collecting validator public keys from participants
- Generating and distributing network genesis files
- Coordinating network launch and operations

#### **Coordination Workflow**

Starting a new network involves coordinating with external validator operators:

1. **Recruit Validators**: Identify organizations or individuals who will run validator nodes on your network
2. **Distribute Instructions**: Share the key generation steps with each validator (see "Creating Network Genesis" section below)
3. **Collect Public Keys**: Each validator generates their private keys securely on their own infrastructure and provides you with their **public key only**
4. **Generate Genesis Files**: Use the collected public keys to create the network genesis files
5. **Distribute Genesis Files**: Share the genesis files with all validators so they can start their nodes
6. **Coordinate Launch**: Ensure all validators start their nodes and connect to each other

**Important**: Validators should **never** share their private keys with you or anyone else. They only provide their public keys for inclusion in the genesis file.

#### **Quick Reference: Roles and Responsibilities**

| **Task** | **Who Does It** | **What They Share** |
|----------|-----------------|---------------------|
| Generate validator private keys | Each validator (independently) | Nothing - keep private! |
| Extract and share public keys | Each validator | Public key only (0x...) |
| Collect all public keys | Network coordinator | N/A |
| Generate genesis files | Network coordinator | Genesis files to all validators |
| Generate PoA admin key | Network coordinator | Nothing - keep private! |
| Distribute genesis files | Network coordinator | Both genesis files to all |
| Configure and run Reth node | All participants | Peer connection info |
| Configure and run Emerald node | All participants | Peer connection info |

---

## **Network Overview**

Required nodes to run for operations:

- Reth - execution client
- Emerald - consensus client

These two services will need to communicate with eachother and with other nodes in the network. Note: for best performance they should be on the same server.

![Network-Diagram](images/network-diagram.png)

---

## **Installing Emerald**

#### **Prerequisites**

- Rust - [https://rust-lang.org/tools/install/](https://rust-lang.org/tools/install/)

First clone the repo:

```
git clone https://github.com/informalsystems/emerald.git
cd emerald
cargo build --release
```

This will build the Emerald binary and place it under `target/release/emerald` which can then be copied to the desired machine under `/usr/local/bin/emerald` for example.

---

## **Installing Reth**

#### **Prerequisites**

- Rust - [https://rust-lang.org/tools/install/](https://rust-lang.org/tools/install/)

First clone the repo:

```
git clone https://github.com/informalsystems/emerald.git
cd emerald/custom-reth
cargo build --release
```

This will build the Emerald binary and place it under `target/release/custom-reth` which can then be copied to the desired machine under `/usr/local/bin/custom-reth` for example.

---

## **Creating Network Genesis**

This section covers the key exchange process between you (the network coordinator) and your validators.

### **Step 1: Instruct Validators to Generate Keys**

As the network coordinator, you need to provide each validator with the following instructions to generate their validator keys **on their own infrastructure**:

#### **Instructions to Send to Validators:**

---

**Validator Key Generation Instructions**

To participate in the network, you need to generate your validator signing keys. Follow these steps:

1. **Install Emerald** (if not already installed):
   ```bash
   git clone https://github.com/informalsystems/emerald.git
   cd emerald
   cargo build --release
   ```

This will build the Emerald binary and place it under `target/release/custom-reth` which can then be copied to the desired machine under `/usr/local/bin/custom-reth` for example.

 **Generate your validator private key**:
   ```bash
   emerald init --home /path/to/home_dir
   ```

   This creates a private key file at `<home_dir>/config/priv_validator_key.json`

   **IMPORTANT**: Keep this file secure and private. Never share this file with anyone, including the network coordinator.

3. **Extract your public key**:
   ```bash
   emerald show-pubkey <home_dir>/config/priv_validator_key.json
   ```

   This will output a public key string like:
   ```
   0xd8620dd478f043bd27fc9389ec6873410265cf8640cb636decd2f0a2ddad7aa5656e58f05b1596a9c737f7073211089c6b49ab7ad5bdb9ab55bf83741b3ee4e4
   ```

4. **Provide your public key to the network coordinator**: Send only this public key string (starting with `0x`) to the network coordinator. Do not send your private key file.

---

### **Step 2: Collect Public Keys from Validators**

Once validators have generated their keys, collect all the public keys they provide. You should receive one public key per validator, each looking like:

```
0xd8620dd478f043bd27fc9389ec6873410265cf8640cb636decd2f0a2ddad7aa5656e58f05b1596a9c737f7073211089c6b49ab7ad5bdb9ab55bf83741b3ee4e4
```

Create a file (e.g., `validator_public_keys.txt`) with one public key per line:

```
0xd8620dd478f043bd27fc9389ec6873410265cf8640cb636decd2f0a2ddad7aa5656e58f05b1596a9c737f7073211089c6b49ab7ad5bdb9ab55bf83741b3ee4e4
0x9b9fc5d66ec179df923dfbb083f2e846ff5da508650c77473c8427fafe481a5e73c1ad26bed12895108f463b84f6dd0d8ebbf4270a06e312a3b63295cffebbff
0x317052004566d1d2ac0b3161313646412f93275599eb6455302a050352027905346eb4a0eebce874c35b1cd29bb5472c46eb2fd9ed24e57c2b73b85b59729e36
```

### **Step 3: Setup PoA (Proof of Authority) Address**

As the network coordinator, you need to create a Proof of Authority (PoA) admin key that will control validator set management (adding/removing/updating validators).

Use your preferred Ethereum key management tool (e.g., MetaMask, cast, or any Ethereum wallet) to generate a new private key. You will need the **address** (e.g., `0x123abc...`) for the next step.

**Important**: This PoA address will have authority over the validator set, so keep the private key secure.

### **Step 4: Generate Genesis Files**

Now that you have collected all validator public keys and have your PoA address, you can generate the genesis files for both Reth and Emerald.

Run the following command with your `validator_public_keys.txt` file:

```
emerald genesis \
  --public-keys-file /path/to/validator_public_keys.txt \
  --chain-id 12345 \
  --poa-owner-address <ADDRESS_GENERATED_IN_PREVIOUS_STEP> \
  --evm-genesis-output ./eth-genesis.json \
  --emerald-genesis-output ./emerald-genesis.json
```

This command takes all the validator public keys and generates:
- **`eth-genesis.json`**: Genesis file for Reth (execution layer), including the PoA smart contract
- **`emerald-genesis.json`**: Genesis file for Emerald (consensus layer)

### **Step 5: Distribute Genesis Files to Validators**

Now you need to share the generated genesis files with all validator participants:

1. **Send the genesis files**: Provide both `eth-genesis.json` and `emerald-genesis.json` to each validator
2. **Share network parameters**: Include the following information:
   - Chain ID (the value you used in the genesis command)
   - JWT secret (which you'll generate in the next section - all nodes must use the same JWT)
   - Peer connection details (IP addresses and ports for other validators)
3. **Coordinate node configurations**: Each validator will need to configure their Reth and Emerald nodes (see sections below)

**Important**: All nodes in the network must use the **same** genesis files. Any difference will result in nodes being unable to reach consensus.

---

## **Running Reth (Execution Client)**

**Note**: This section applies to **all network participants** (both the coordinator and all validators). Each validator must run their own Reth node.

Reth is the Ethereum execution client. It handles transaction execution, state management, and provides JSON-RPC endpoints for interacting with the blockchain.

#### **Prerequisites**

- Reth binary installed (custom-reth installed from previous step)
- Genesis file (`eth-genesis.json`) created for your network from previous step.

<br/><br/>

#### **Generate JWT Secret**

The JWT secret is required for authenticated communication between Reth (execution client) and Emerald (consensus engine) via the Engine API.

**For the Network Coordinator**: Generate a single JWT secret and share it with all validators:

```bash
openssl rand -hex 32
```

Save this hex string to a file (e.g., `jwt.hex`) and distribute it to all validators.

**Important**:
- The same JWT must be used by **both** Reth and Emerald on each node
- **All validators** must use the **same JWT secret** (share the hex string with all participants)
- Each node should save the JWT hex string to a file and reference it in both Reth and Emerald configurations

<br/><br/>

#### **Start Reth Node**

Start Reth with the following configuration:

```bash
custom-reth node \
  --chain /path/to/genesis.json \
  --datadir /var/lib/reth \
  --http \
  --http.addr 0.0.0.0 \
  --http.port 8545 \
  --http.api eth,net,web3,txpool,debug \
  --http.corsdomain "*" \
  --ws \
  --ws.addr 0.0.0.0 \
  --ws.port 8546 \
  --ws.api eth,net,web3,txpool,debug \
  --authrpc.addr 0.0.0.0 \
  --authrpc.port 8551 \
  --authrpc.jwtsecret /var/lib/reth/jwt.hex \
  --port 30303 \
  --metrics=0.0.0.0:9000
```

<br/><br/>

#### **Key Configuration Options**

- `--chain`: Path to genesis configuration file
- `--datadir`: Database and state storage directory
- `--http.*`: JSON-RPC HTTP endpoint configuration
- `--ws.*`: WebSocket endpoint configuration
- `--authrpc.*`: Authenticated Engine API for consensus client communication
- `--authrpc.jwtsecret`: Path to JWT secret file for Engine API authentication (must match Emerald's JWT)
- `--port`: P2P networking port for peer connections
- `--disable-discovery`: Disable peer discovery (useful for permissioned networks)

<br/><br/>

#### **Network Endpoints**

Once running, Reth provides the following endpoints:

- **HTTP RPC**: `http://<IP>:8545` - Standard Ethereum JSON-RPC
- **WebSocket**: `ws://<IP>:8546` - WebSocket subscriptions
- **Engine API**: `http://<IP>:8551` - Authenticated API for Emerald consensus
- **Metrics**: `http//<IP>:9000` - Prometheus Metrics Endpoint

<br/><br/>

#### **Configuring Reth Peer Connections**

For a multi-validator network, Reth nodes need to connect to each other to sync the blockchain state and propagate transactions. This section explains how to establish peer connections between all Reth nodes.

**Why Peering is Important:**
- Enables block and transaction propagation across the network
- Allows nodes to stay synchronized with each other
- Creates a resilient network topology

<br/>

##### **Method 1: Using the `--trusted-peers` Flag (Recommended)**

This is the recommended approach as it automatically establishes connections when nodes start up.

**Step 1: Each Validator Gets Their Enode URL**

Each validator needs to obtain their node's enode URL. The enode URL contains the node's identity and network address.

To get your enode URL, start your Reth node first (with `admin` added to `--http.api`), then run:

```bash
curl -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"admin_nodeInfo","params":[],"id":1}' \
  http://localhost:8545 | jq -r '.result.enode'
```

This will output something like:
```
enode://a0fd9e095d89320c27b2a07460f4046f63747e5b99ca14dd94475f65910bf0c67037fc1194a04d083afb13d61def3f6f1112757f514ca2fdabd566610658d030@127.0.0.1:30303
```

**Important**: Replace `127.0.0.1` with your server's **public IP address** before sharing. For example:
```
enode://a0fd9e095d89320c27b2a07460f4046f63747e5b99ca14dd94475f65910bf0c67037fc1194a04d083afb13d61def3f6f1112757f514ca2fdabd566610658d030@203.0.113.10:30303
```

**Step 2: Network Coordinator Collects All Enode URLs**

As the network coordinator, collect the enode URLs from all validators and compile them into a single list.

**Step 3: Distribute Peer List to All Validators**

Share the complete list of enode URLs with all validators. Each validator should add the other validators' enodes (excluding their own) to their Reth startup command using the `--trusted-peers` flag:

```bash
custom-reth node \
  --chain /path/to/genesis.json \
  --datadir /var/lib/reth \
  --http \
  --http.addr 0.0.0.0 \
  --http.port 8545 \
  --http.api eth,net,web3,txpool,debug \
  --authrpc.addr 0.0.0.0 \
  --authrpc.port 8551 \
  --authrpc.jwtsecret /var/lib/reth/jwt.hex \
  --port 30303 \
  --metrics=0.0.0.0:9000 \
  --trusted-peers=enode://PEER1_ENODE@PEER1_IP:30303,enode://PEER2_ENODE@PEER2_IP:30303,enode://PEER3_ENODE@PEER3_IP:30303
```

**Example with actual values:**
```bash
--trusted-peers=enode://a0fd9e095d89320c27b2a07460f4046f63747e5b99ca14dd94475f65910bf0c67037fc1194a04d083afb13d61def3f6f1112757f514ca2fdabd566610658d030@203.0.113.10:30303,enode://add24465ccee48d97a0212afde6b2c0373c8b2b37a1f44c46be9d252896fe6c55256fd4bd8652cf5d41a11ffae1f7537922810b160a4fd3ed0c6f388d137587e@203.0.113.11:30303
```

**Notes:**
- Each validator excludes their own enode from their `--trusted-peers` list
- All enodes should use the **public IP addresses** of the validator servers
- Make sure port 30303 (or your configured P2P port) is open in firewalls between validators

<br/>

##### **Method 2: Adding Peers at Runtime (Alternative)**

If you need to add peers to an already-running node, you can use the JSON-RPC API:

**Prerequisites:**
- Add `admin` to the `--http.api` flag when starting Reth
- This must be done on all Reth nodes that will use this method

**Add a trusted peer:**
```bash
curl -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"admin_addTrustedPeer","params":["enode://FULL_ENODE_URL"],"id":1}' \
  http://localhost:8545
```

**Example:**
```bash
curl -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"admin_addTrustedPeer","params":["enode://a0fd9e095d89320c27b2a07460f4046f63747e5b99ca14dd94475f65910bf0c67037fc1194a04d083afb13d61def3f6f1112757f514ca2fdabd566610658d030@203.0.113.10:30303"],"id":1}' \
  http://localhost:8545
```

**Verify peer connections:**
```bash
curl -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"admin_peers","params":[],"id":1}' \
  http://localhost:8545 | jq
```

**Drawback**: Peers added this way are not persisted across restarts. Use Method 1 for production deployments.

<br/><br/>

#### **Systemd Service**

For remote deployments, you can use systemd to manage the Reth process. See [reth.systemd.service.example](config-examples/reth.systemd.server.example) for a service configuration example.

---

## **Running Emerald (Consensus Engine)**

**Note**: This section applies to **all network participants** (both the coordinator and all validators). Each validator must run their own Emerald node with the private key they generated earlier.

Emerald is the consensus client, built on Malachite BFT. It coordinates with Reth via the Engine API to produce blocks and achieve consensus across the validator network.

#### **Prerequisites**

- Emerald binaries built `emerald` (`cargo build --release` where binary can be found in path `target/release/emerald`)
- Node configuration directory created (contains `config.toml`, `emerald.toml`, and `priv_validator_key.json`) - Recommended to setup a user `emerald` and use a home folder like `/home/emerald/.emerald` and in there a config folder for all files.
- Reth node must be running with Engine API enabled
- JWT secret file (same as used by Reth)

<br/><br/>

#### **Configuration Files**

Each Emerald node requires two configuration files in its home directory:

**1. `config.toml` (MalachiteBFT Configuration)**

See [malachitebft-config.toml](config-examples/malachitebft-config.toml) for a complete example. Key sections:

- **Consensus settings**: Block timing, timeouts, and consensus parameters
- **P2P networking**: Listen addresses and peer connections
  - Consensus P2P: Port 27000 (default)
    -  persistent_peers must be filled out for p2p
  - Mempool P2P: Port 28000 (default)
    -  persistent_peers must be filled out for p2p
- **Metrics**: Prometheus metrics endpoint on port 30000

This file must be in config folder in home_dir, example `/home/emerald/.emerald/config/config.toml` where --home flag would be defined as `--home=/home/emerald/.emerald`

**2. `emerald.toml` (Execution Integration)**

See [emerald-config.toml](config-examples/emerald-config.toml) for a complete example. Key settings:

```toml
moniker = "validator-0"
execution_authrpc_address = "http://<RETH_IP>:8545"
engine_authrpc_address = "http://<RETH_IP>:8551"
jwt_token_path = "/path/to/jwt.hex"
el_node_type = "archive"
sync_timeout_ms = 1000000
sync_initial_delay_ms = 100
...
```

**Important**: The `jwt_token_path` must point to the same JWT token used by Reth.

<br/><br/>

#### **Configure Peer Connections**

For a multi-node network, configure persistent peers in `config.toml`:

```toml
[consensus.p2p]
listen_addr = "/ip4/0.0.0.0/tcp/27000"
persistent_peers = [
    "/ip4/<PEER1_IP>/tcp/27000",
    "/ip4/<PEER2_IP>/tcp/27000",
    "/ip4/<PEER3_IP>/tcp/27000",
]

[mempool.p2p]
listen_addr = "/ip4/0.0.0.0/tcp/28000"
persistent_peers = [
    "/ip4/<PEER1_IP>/tcp/28000",
    "/ip4/<PEER2_IP>/tcp/28000",
    "/ip4/<PEER3_IP>/tcp/28000",
]
```

Replace `<PEER_IP>` with the actual IP addresses of your validator peers.

<br/><br/>

#### **Start Emerald Node**

Start the Emerald consensus node:

```bash
emerald start \
  --home /home/emerald/.emerald \
  --config /home/emerald/.emerald/config/emerald.toml \
  --log-level info
```

The `--home` directory should contain:
- `<home>/config/config.toml` - Malachite BFT configuration
- `<home>/config/priv_validator_key.json` - Validator signing key
- `<home>/config/genesis.json` - Malachite BFT genesis file

An example Malachite BFT config file is provided: [malachitebft-config.toml](malachitebft-config.toml)

The `--config` flag should contain the explicit file path to the Emerald config:
- Example: `--config=/home/emerald/.emerald/config/emerald.toml`

<br/><br/>

#### **Emerald Config**

A sample Emerald config file has been provided: [emerald-config.toml](emerald-config.toml). 

This is where you define how Emerald connects to Reth. Make sure to fill in the Reth http and authrpc address.

Also make sure to place the JWT token that Reth is using in the JWT file path in Emerald config file, this JWT token needs to be the same so they can communicate.

<br/><br/>

#### **Key Ports**

- **27000**: Consensus P2P communication (which port to accept incoming traffic on)
- **28000**: Mempool P2P communication  (which port to accept incoming traffic on)
- **30000**: Prometheus metrics endpoint port

<br/><br/>

#### **Peering**

In the Malachite BFT config.toml you will need to fill in the 2 sections (consensus.p2p and mempool.p2p) `persistent_peers` array.
It uses the format `/ip4/<IP_ADDRESS_TO_REMOTE_PEER>/tcp/<PORT_FOR_REMOTE_PEER>`. Make sure to fill in all peers in the testnet.

<br/><br/>

#### **Monitoring**

Emerald exposes Prometheus metrics on port 30000 (configurable in `config.toml`):

```bash
curl http://<IP>:30000/metrics
```

<br/><br/>

#### **Systemd Service**

For production deployments, use systemd to manage the Emerald process. See [emerald.systemd.service.example](config-examples/emerald.systemd.service.example) for a complete service configuration.

---

### Security Notes

- Make sure no ports are exposed to the internet and all traffic is secured with VPCs or VPN tunnels.

