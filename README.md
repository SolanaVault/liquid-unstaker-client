# CLI for The Vault Liquid Unstaker program

This CLI targets v3 of the Liquid Unstaker protocol.

## User commands

- `quote-unstake-lst`, quote instant unstake of an LST into native SOL
- `quote-unstake-lst-wrapped`, quote instant unstake of an LST into wrapped SOL
- `unstake-lst`, instant unstake an SPL stake-pool LST into native SOL
- `unstake-lst-wrapped`, instant unstake an SPL stake-pool LST into wrapped SOL
- `quote-sell-lst`, quote the v3 marketplace `sell_lst` path
- `sell-lst`, sell LST tokens directly to the pool for wrapped SOL
- `quote-buy-lst`, quote the v3 marketplace `buy_lst` path
- `buy-lst`, buy LST tokens from pool inventory with wrapped SOL
- `deposit`, deposit SOL into the pool and receive LP tokens
- `withdraw`, burn LP tokens and withdraw SOL from the pool
- `withdraw-stake-account`, burn LP tokens and receive a pool-owned deactivating stake account split
- `unstake-stake-account`, instant unstake a user-owned stake account into native SOL

## Operator commands

- `initialize-pool`, initialize a v3 pool at the supplied pool PDA
- `upsert-lst-info`, create/update a v3 LST allowlist entry
- `sync-inventory`, refresh the v3 inventory summary for active LST inventory
- `unstake-pool-lsts`, authority-only batch unstake of pool-owned LST inventory
- `list-pool-lsts`, list non-zero pool-owned LST balances from configured LST info entries
- `update`, harvest withdrawable lamports from tracked deactivating stake accounts
- `update-pool`, authority-only pool config update and v2-to-v3 account resize
- `halt-pool`, authority-only halt/unhalt of user-facing pool instructions
- `create-or-update-token-metadata`, create/update LP token metadata
- `list-lst-info`, list configured LST allowlist entries
- `list-lst-mints`, discover SPL stake-pool mints from supported stake-pool programs
- `pool-info`, print decoded pool and inventory summary accounts

Run command-specific help with:

```sh
liquid-unstaker-client-cli --help
liquid-unstaker-client-cli --pool <POOL> --rpc <RPC_URL> sell-lst --help
liquid-unstaker-client-cli --pool <POOL> --rpc <RPC_URL> unstake-pool-lsts --help
```

Use `--dump-transaction-message` with any transaction command to print the base58-encoded Solana
message bytes instead of signing or sending. When dumping, `--keypair` may be either a keypair file
or the signer pubkey, which is useful for external signing flows such as Ledger.

## Examples

### Quote a v3 LST sale

```sh
liquid-unstaker-client-cli \
  --pool 9nyw5jxhzuSs88HxKJyDCsWBZMhxj2uNXsFcyHF5KBAb \
  --rpc "$RPC_URL" \
  --keypair "$KEYPAIR_PATH" \
  quote-sell-lst vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7 10000000
```

### Sell an LST to the pool for wrapped SOL

```sh
liquid-unstaker-client-cli \
  --pool 9nyw5jxhzuSs88HxKJyDCsWBZMhxj2uNXsFcyHF5KBAb \
  --rpc "$RPC_URL" \
  --keypair "$KEYPAIR_PATH" \
  sell-lst vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7 10000000 --min-lamports-out 9900000
```

### Buy LST inventory from the pool with wrapped SOL

The wallet's WSOL associated token account is used as the payment source.

```sh
liquid-unstaker-client-cli \
  --pool 9nyw5jxhzuSs88HxKJyDCsWBZMhxj2uNXsFcyHF5KBAb \
  --rpc "$RPC_URL" \
  --keypair "$KEYPAIR_PATH" \
  buy-lst vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7 10000000 --max-lamports-in 11000000
```

### Enable an LST for v3 trading

The stake-pool account can be omitted if it can be discovered from the mint.

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  --keypair "$AUTHORITY_KEYPAIR_PATH" \
  upsert-lst-info <LST_MINT>
```

### Sync inventory

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  --keypair "$MAINTENANCE_AUTHORITY_KEYPAIR_PATH" \
  sync-inventory --chunk-size 8
```

### Inventory status

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  inventory-status
```

### List pool LST inventory

Prints CSV rows as `mint,amount` for configured LST info entries where the pool-owned token balance is greater than zero.

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  list-pool-lsts
```

### Unstake pool LST inventory

`ALL` is accepted case-insensitively in the `mint` and `amount` positions.

Use `ALL` as the amount to unstake the pool's full non-zero balance for one mint:

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  --keypair "$MAINTENANCE_AUTHORITY_KEYPAIR_PATH" \
  unstake-pool-lsts <LST_MINT> ALL
```

Use `ALL` as the mint to unstake the same amount from every non-zero pool-owned LST balance. The CLI checks every selected mint before sending and errors if any balance is below the requested amount.

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  --keypair "$MAINTENANCE_AUTHORITY_KEYPAIR_PATH" \
  unstake-pool-lsts ALL <AMOUNT>
```

Use `ALL ALL` to unstake all non-zero pool-owned LST balances:

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  --keypair "$MAINTENANCE_AUTHORITY_KEYPAIR_PATH" \
  unstake-pool-lsts ALL ALL
```

When unstaking multiple mints with an explicit `--stake-account-seed`, the CLI advances the seed between transactions so derived stake-account PDAs are not reused.

### Update pool config

`update-pool` rewrites the v3 pool configuration and requires the maintenance authority value to store on the pool.

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  --keypair "$AUTHORITY_KEYPAIR_PATH" \
  update-pool \
  --manager-fee-account <MANAGER_FEE_ACCOUNT> \
  --fee-max <FEE_MAX> \
  --fee-min <FEE_MIN> \
  --min-sol-for-min-fee <LAMPORTS> \
  --manager-fee-pct <PCT> \
  --vault-lamports-cap <LAMPORTS> \
  --withdraw-sol-fee <FEE> \
  --withdraw-stake-account-fee <FEE> \
  --flash-loans-enabled <true|false> \
  --flash-loan-fee <FEE> \
  --sell-lst-flat-fee <FEE> \
  --buy-lst-flat-fee <FEE> \
  --buy-lst-dynamic-fee-max <FEE> \
  --expected-inflation-per-epoch <FEE> \
  --max-epoch-progress-pct <PCT> \
  --min-buy-lamports <LAMPORTS> \
  --max-rate-drift-bps <BPS> \
  --maintenance-authority <MAINTENANCE_AUTHORITY>
```

### Harvest deactivating stake accounts

Omit `--stake-account` to discover and process all tracked stake accounts for the pool. The CLI processes updates in chunks of eight stake accounts by default; override with `--chunk-size` when needed.

```sh
liquid-unstaker-client-cli \
  --pool <POOL> \
  --rpc "$RPC_URL" \
  --keypair "$PAYER_KEYPAIR_PATH" \
  update --chunk-size 8
```
