# CLI for The Vault Liquid Unstaker program

Available commands in the CLI

- quote-unstake-lst, get a quote in lamports of how much would be received for a given amount of LST tokens
- unstake-lst, perform a liquid unstake of an SPL LST, e.g. jitoSOL, vSOL, bSOL. Eseentially send LST amount and and receive naked SOL back. Conversion rate is determined by the SPL stake pool
- unstake-stake (TODO)
- deposit SOL (TODO)
- withdraw SOL (TODO)

## Examples:

### Get a quote for 0.01 vSOL from the main pool

```
liquid-unstaker-client-cli --pool 9nyw5jxhzuSs88HxKJyDCsWBZMhxj2uNXsFcyHF5KBAb --rpc $RPC_URL --keypair $KEYPAIR_PATH quote-unstake-lst  vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7 10000000
```

### Unstake 0.01 vSOL from the main pool

```
liquid-unstaker-client-cli --pool 9nyw5jxhzuSs88HxKJyDCsWBZMhxj2uNXsFcyHF5KBAb --rpc $RPC_URL --keypair $KEYPAIR_PATH unstake-lst  vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7 10000000
```
