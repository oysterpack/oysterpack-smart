## Running a Validator Node on testnet
- https://docs.near.org/docs/validator/staking
```shell
STAKING_POOL=dev-1618770943926-8326158
NEARCORE=~/Documents/projects/github/near/nearcore

nearup run testnet --binary-path $NEARCORE/target/release --account-id $STAKING_POOL
```

## Running a Validator Node on mainnet
- https://docs.near.org/docs/validator/deploy-on-mainnet
```shell
git clone https://github.com/near/nearcore.git
export NEAR_RELEASE_VERSION=$(curl -s https://github.com/near/nearcore/releases/latest | tr '/" ' '\n' | grep "[0-9]\.[0-9]*\.[0-9]" | head -n 1)
echo $NEAR_RELEASE_VERSION
cd nearcore
git checkout -b $NEAR_RELEASE_VERSION
cargo build -p neard --release

target/release/neard init --chain-id="mainnet" --account-id=<YOUR_STAKING_POOL_ID>
target/release/neard run
```

## Initializing the STAKE contract
```shell
cd near/oysterpack-smart-stake
# set `CONTRACT_NAME` env var
. ./neardev/dev-account.env
echo $CONTRACT_NAME
near call $CONTRACT_NAME deploy --accountId oysterpack.testnet --args '{"stake_public_key":"ed25519:AC1pVDXsE8sZiLAqLTDa3sD8DH74U5yUDaYKWeBwwyJj"}'
```

## Setting up the validator node
- https://www.digitalocean.com/community/tutorials/how-to-configure-ssh-key-based-authentication-on-a-linux-server

### Digital Ocean - GuildNet
- droplet: oysterpack-validator-guildnet-1
```shell
iptables -t nat -A PREROUTING -p tcp --dport 24567 -j DNAT --to-destination 10.195.213.223:24567
lxc config device add validator port24567 proxy listen=tcp:0.0.0.0:25567 connect=tcp:127.0.0.1:24567
```
