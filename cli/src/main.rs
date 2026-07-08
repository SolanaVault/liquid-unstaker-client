#![allow(clippy::too_many_arguments)]

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Write},
    mem::{offset_of, size_of},
    ops::{Div, Mul},
    rc::Rc,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anchor_client::{
    solana_client::{
        nonblocking::rpc_client::RpcClient,
        rpc_config::{
            RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionAccountsConfig,
            RpcSimulateTransactionConfig,
        },
        rpc_filter::{Memcmp, RpcFilterType},
    },
    solana_sdk::{
        self,
        instruction::{AccountMeta, Instruction},
        message::Message,
        program_pack::Pack,
        pubkey::Pubkey,
        signature::{read_keypair_file, Keypair},
        signer::Signer,
        system_instruction::create_account,
        transaction::Transaction,
    },
    Client,
};
use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token, token::spl_token,
    token_interface::spl_token_metadata_interface::borsh::BorshDeserialize,
};
use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command};
use fee::{Fee, FEE_PCT_DIVISOR};
use itertools::{izip, Itertools};
use liquid_unstaker::liquid_unstaker::{accounts as lu_accounts, client as lu_client, ID_CONST};
use serde::Deserialize;
use solana_account_decoder::{UiAccount, UiAccountEncoding};
use spl_stake_pool::{
    find_stake_program_address,
    state::{StakePool, StakeStatus},
};

mod error;
mod fee;

const SANCTUM_SINGLE_VALIDATOR_STAKE_POOL_PROGRAM: Pubkey =
    pubkey!("SP12tWFxD9oJsVWNavTTBZvMbA6gkAmxtVgxdqvyvhY");
const SANCTUM_MULTIPLE_VALIDATORS_STAKE_POOL_PROGRAM: Pubkey =
    pubkey!("SPMBzsVUuoHA4Jm6KunbsotaahvVikZs1JyTW6iJvbn");

const SUPPORTED_STAKE_POOL_PROGRAMS: [Pubkey; 3] = [
    spl_stake_pool::id(),
    SANCTUM_SINGLE_VALIDATOR_STAKE_POOL_PROGRAM,
    SANCTUM_MULTIPLE_VALIDATORS_STAKE_POOL_PROGRAM,
];

const INFLATION_PCT_DIVISOR: u64 = 1_000_000_000;
const RATE_SCALE: u128 = 1_000_000_000;
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
const TOKEN_DECIMAL_FACTOR: u128 = 1_000_000_000;
const MAX_WITHDRAW_FEE: u16 = (FEE_PCT_DIVISOR / 100) as u16;
const BUY_REFERENCE_INVENTORY: u64 = 1_000_000_000_000;
const BUY_ACTIVITY_SCALING: u64 = 100;
const DEFAULT_UPDATE_CHUNK_SIZE: usize = 8;
const DEFAULT_COMPARE_SOL_LAMPORTS: [u64; 1] = [1_000_000_000];
const DEFAULT_JUPITER_BUILD_URL: &str = "https://api.jup.ag/swap/v2/build";
const DEFAULT_JUPITER_EXCLUDED_DEX: &str = "VaultLiquidUnstake";
const PERCENT_SCALE: u128 = 1_000_000;
const PERCENT_UNITS_PER_ONE_PERCENT: u128 = PERCENT_SCALE / 100;
const BALANCED_LST_CAP_TRIGGER_BUFFER_PERCENT: u32 = 1_000;
const MPL_TOKEN_METADATA_PROGRAM: Pubkey = pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");
const ANCHOR_DISCRIMINATOR_LEN: usize = 8;
const PUBKEY_DATA_LEN: usize = 32;
const LST_INFO_POOL_OFFSET: usize = ANCHOR_DISCRIMINATOR_LEN;
const LST_INFO_MINT_OFFSET: usize = LST_INFO_POOL_OFFSET + PUBKEY_DATA_LEN;
const LST_INFO_STAKE_POOL_OFFSET: usize = LST_INFO_MINT_OFFSET + PUBKEY_DATA_LEN;
const LST_INFO_STAKE_POOL_PROGRAM_OFFSET: usize = LST_INFO_STAKE_POOL_OFFSET + PUBKEY_DATA_LEN;
const LST_INFO_BUMP_OFFSET: usize = LST_INFO_STAKE_POOL_PROGRAM_OFFSET + PUBKEY_DATA_LEN;
const LST_INFO_IS_ACTIVE_OFFSET: usize = LST_INFO_BUMP_OFFSET + 1;
const LST_INFO_V3_DATA_LEN: usize =
    ANCHOR_DISCRIMINATOR_LEN + 32 + 32 + 32 + 32 + 1 + 1 + 4 + 1 + 1 + 5 * 8 + 5 * 8 + 1;
const INVENTORY_SYNC_ACCOUNTS_PER_ENTRY: usize = 2;
const INVENTORY_SYNC_ENTRIES_PER_RPC_BATCH: usize = 100 / INVENTORY_SYNC_ACCOUNTS_PER_ENTRY;
static DUMP_TRANSACTION_MESSAGE: AtomicBool = AtomicBool::new(false);

type ProgramClient = anchor_client::Program<Rc<Keypair>>;
type PoolAccount = lu_accounts::Pool;
type LstInfoAccount = lu_accounts::LstInfo;
type InventorySummaryAccount = lu_accounts::InventorySummary;
type StakeAccountInfoAccount = lu_accounts::StakeAccountInfo;

enum Wallet {
    Keypair(Keypair),
    Pubkey(Pubkey),
}

impl Wallet {
    fn pubkey(&self) -> Pubkey {
        match self {
            Wallet::Keypair(k) => k.pubkey(),
            Wallet::Pubkey(p) => *p,
        }
    }

    fn keypair(&self, context: &str) -> Result<&Keypair> {
        match self {
            Wallet::Keypair(k) => Ok(k),
            Wallet::Pubkey(p) => Err(anyhow!(
                "{context} requires a local keypair, but --keypair was provided as pubkey {p}"
            )),
        }
    }
}

enum PubkeyOrKeypair {
    Pubkey(Pubkey),
    Keypair(Keypair),
}

impl PubkeyOrKeypair {
    fn pubkey(&self) -> Pubkey {
        match self {
            PubkeyOrKeypair::Pubkey(p) => *p,
            PubkeyOrKeypair::Keypair(k) => k.pubkey(),
        }
    }
}

type UnstakeAccountSelection = (Vec<u64>, Vec<Pubkey>, Vec<PubkeyOrKeypair>, Vec<Pubkey>);

#[derive(Clone, Copy)]
enum PoolLstMintSelection {
    One(Pubkey),
    All,
}

#[derive(Clone, Copy)]
enum PoolLstAmountSelection {
    Amount(u64),
    All,
}

#[derive(Clone, Copy, Debug)]
struct PoolLstTargetOverride {
    mint: Pubkey,
    percent: u32,
}

#[derive(Clone, Copy, Debug)]
struct PoolLstUnstakeRequest {
    mint: Pubkey,
    amount: u64,
    stake_pool_program_id: Option<Pubkey>,
}

#[derive(Clone, Debug)]
struct PoolLstPositionSnapshot {
    mint: Pubkey,
    amount: u64,
    sol_value: u64,
    stake_pool_program_id: Pubkey,
    stake_pool_total_lamports: u64,
    stake_pool_token_supply: u64,
    stake_withdrawal_fee: spl_stake_pool::state::Fee,
}

#[derive(Clone, Debug)]
struct BalancedPoolLstPlanPosition {
    mint: Pubkey,
    current_amount: u64,
    current_sol_value: u64,
    current_sol_pct: u32,
    target_amount: u64,
    target_sol_value: u64,
    target_sol_pct: u32,
    unstake_amount: u64,
    unstake_sol_lamports: u64,
    override_percent: Option<u32>,
    stake_pool_program_id: Pubkey,
    note: Option<String>,
}

#[derive(Debug)]
struct BalancedPoolLstPlan {
    cap_percent: u32,
    trigger_percent: u32,
    sol_vault_lamports: u64,
    total_deactivating_stake_lamports: u64,
    current_lst_value_lamports: u128,
    target_lst_value_lamports: u128,
    trigger_lst_value_lamports: u128,
    new_lst_value_lamports: u128,
    tvl_lamports: u128,
    minimum_unstake_lamports: u64,
    positions: Vec<BalancedPoolLstPlanPosition>,
}

#[derive(Debug)]
struct SellQuote {
    total_sol_value: u64,
    base_fee_pct: u64,
    base_flat_fee: u64,
    stake_pool_fee: u64,
    total_fee: u64,
    manager_fee: u64,
    pool_fee: u64,
    amount_to_user: u64,
}

#[derive(Debug)]
struct BuyQuote {
    total_sol_value_without_discount: u64,
    half_stake_pool_fee_pct: u64,
    lst_cost: u64,
    dynamic_fee_pct: u32,
    pool_fee: u64,
    manager_fee: u64,
    total_fee: u64,
    total_cost: u64,
    user_wsol_account_rent: u64,
    user_lst_account_rent: u64,
    estimated_transaction_fee: u64,
    estimated_wallet_lamports_out: u64,
    simulated_protocol_total_cost: u64,
    simulated_wallet_lamports_out: u64,
    simulated_user_wsol_amount_in: u64,
    simulated_user_lst_amount_out: u64,
}

#[derive(Debug)]
struct ProtocolBuyQuote {
    total_sol_value_without_discount: u64,
    half_stake_pool_fee_pct: u64,
    lst_cost: u64,
    dynamic_fee_pct: u32,
    pool_fee: u64,
    manager_fee: u64,
    total_fee: u64,
    total_cost: u64,
}

#[derive(Clone, Copy, Debug)]
enum CompareDirection {
    SolToLst,
    LstToSol,
}

impl CompareDirection {
    fn label(self) -> &'static str {
        match self {
            CompareDirection::SolToLst => "sol_to_lst",
            CompareDirection::LstToSol => "lst_to_sol",
        }
    }
}

struct CompareRecord {
    timestamp_unix: u64,
    pool: Pubkey,
    mint: Pubkey,
    direction: CompareDirection,
    notional_sol_lamports: u64,
    lst_amount: Option<u64>,
    jupiter_sol_lamports: Option<u64>,
    jupiter_lst_amount: Option<u64>,
    v3_sol_lamports: Option<u64>,
    v3_lst_amount: Option<u64>,
    v3_advantage_lamports: Option<i128>,
    v3_advantage_bps: Option<f64>,
    jupiter_route: Vec<String>,
    error: Option<String>,
}

impl CompareRecord {
    fn success(&self) -> bool {
        self.error.is_none()
    }
}

struct JupiterBuildClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    excluded_dexes: Vec<String>,
    taker: Pubkey,
    request_delay: Duration,
    max_retries: u64,
}

struct JupiterQuote {
    in_amount: u64,
    out_amount: u64,
    route_labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JupiterBuildQuoteResponse {
    in_amount: String,
    out_amount: String,
    #[serde(default)]
    route_plan: Vec<JupiterRoutePlanStep>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JupiterRoutePlanStep {
    swap_info: JupiterSwapInfo,
}

#[derive(Debug, Deserialize)]
struct JupiterSwapInfo {
    label: String,
}

struct AccountSimulationDelta {
    pre_balance: u64,
    post_balance: u64,
    pre_token_amount: Option<u64>,
    post_token_amount: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let matches = Command::new("Liquid Unstaker Client")
        .version("0.2")
        .arg(
            Arg::new("pool")
                .long("pool")
                .help("The liquid unstake pool account")
                .required(true),
        )
        .arg(
            Arg::new("simulate")
                .long("simulate")
                .help("Simulate the transaction without sending it")
                .action(ArgAction::SetTrue)
                .required(false),
        )
        .arg(
            Arg::new("dump-transaction-message")
                .long("dump-transaction-message")
                .help("Print the base58-encoded transaction message instead of signing or sending it. With this flag, --keypair may be a signer pubkey")
                .action(ArgAction::SetTrue)
                .conflicts_with("simulate")
                .required(false),
        )
        .arg(
            Arg::new("no-stake-account-as-pda")
                .long("no-stake-account-as-pda")
                .help("Create new stake accounts as ephemeral keypairs instead of program PDAs")
                .action(ArgAction::SetTrue)
                .required(false),
        )
        .arg(
            Arg::new("rpc")
                .long("rpc")
                .help("The Solana RPC URL")
                .required(true),
        )
        .arg(
            Arg::new("keypair")
                .long("keypair")
                .help("Wallet for transactions"),
        )
        .subcommand(
            Command::new("deposit")
                .about("Deposit SOL into the liquid unstake pool and receive LP tokens")
                .arg(u64_pos_arg("lamports", "Amount to deposit in lamports")),
        )
        .subcommand(
            Command::new("initialize-pool")
                .about("Authority-only: initialize a pool at the PDA passed with --pool")
                .arg(pubkey_arg("manager-fee-account", "Manager fee account"))
                .arg(u32_arg("fee-max", "Maximum unstake fee in fee units"))
                .arg(u32_arg("fee-min", "Minimum unstake fee in fee units"))
                .arg(u64_arg(
                    "min-sol-for-min-fee",
                    "SOL vault lamports needed to reach minimum unstake fee",
                ))
                .arg(u8_arg("manager-fee-pct", "Percent of fees paid to manager"))
                .arg(u64_arg("vault-lamports-cap", "Pool value cap in lamports"))
                .arg(u16_arg("withdraw-sol-fee", "LP SOL withdrawal fee in fee units"))
                .arg(u16_arg(
                    "withdraw-stake-account-fee",
                    "LP stake-account withdrawal fee in fee units",
                ))
                .arg(bool_arg("flash-loans-enabled", "Whether flash loans are enabled"))
                .arg(u32_arg("flash-loan-fee", "Flash loan fee in fee units"))
                .arg(u64_arg(
                    "min-buy-lamports",
                    "Minimum total buy_lst cost in lamports. Use 0 to disable",
                )),
        )
        .subcommand(
            Command::new("withdraw")
                .about("Withdraw SOL from the pool by burning LP tokens")
                .arg(u64_pos_arg("tokens", "LP token amount to burn")),
        )
        .subcommand(
            Command::new("withdraw-stake-account")
                .about("Withdraw from the pool as a stake account by burning LP tokens")
                .arg(u64_pos_arg("tokens", "LP token amount to burn"))
                .arg(pubkey_pos_arg(
                    "stake-account-source",
                    "Pool-owned deactivating stake account to split from",
                ))
                .arg(
                    Arg::new("destination-keypair")
                        .long("destination-keypair")
                        .help("Keypair for the destination stake account returned to the user")
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("unstake-stake-account")
                .about("Instant unstake a user-owned stake account into native SOL")
                .arg(pubkey_pos_arg("stake-account", "Stake account to liquid unstake"))
                .arg(optional_u64_arg(
                    "min-lamports-out",
                    "Minimum lamports out for slippage protection",
                )),
        )
        .subcommand(
            Command::new("unstake-lst")
                .about("Instant unstake an SPL stake-pool LST into native SOL")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount"))
                .arg(optional_u64_arg(
                    "min-lamports-out",
                    "Minimum lamports out for slippage protection",
                )),
        )
        .subcommand(
            Command::new("unstake-lst-wrapped")
                .about("Instant unstake an SPL stake-pool LST into wrapped SOL")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount"))
                .arg(optional_u64_arg(
                    "min-lamports-out",
                    "Minimum wrapped lamports out for slippage protection",
                )),
        )
        .subcommand(
            Command::new("quote-unstake-lst")
                .about("Quote instant unstake into native SOL")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount")),
        )
        .subcommand(
            Command::new("quote-unstake-lst-wrapped")
                .about("Quote instant unstake into wrapped SOL")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount")),
        )
        .subcommand(
            Command::new("sell-lst")
                .about("Sell LST inventory to the pool for wrapped SOL")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount to sell"))
                .arg(optional_u64_arg(
                    "min-lamports-out",
                    "Minimum wrapped lamports out for slippage protection",
                )),
        )
        .subcommand(
            Command::new("buy-lst")
                .about("Buy LST inventory from the pool with wrapped SOL")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount to buy"))
                .arg(optional_u64_arg(
                    "max-lamports-in",
                    "Maximum WSOL lamports in for slippage protection",
                )),
        )
        .subcommand(
            Command::new("quote-sell-lst")
                .about("Quote v3 sell_lst")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount to sell")),
        )
        .subcommand(
            Command::new("quote-buy-lst")
                .about("Quote v3 buy_lst funded from the user WSOL account")
                .arg(string_arg("mint", "LST mint"))
                .arg(u64_pos_arg("amount", "LST token amount to buy")),
        )
        .subcommand(
            Command::new("compare-price")
                .about("Compare Jupiter SOL/LST quotes against v3 pool buy/sell quotes")
                .arg(
                    Arg::new("amount-sol")
                        .long("amount-sol")
                        .help("SOL notional to compare. Repeat to override the default: 1")
                        .action(ArgAction::Append)
                        .required(false),
                )
                .arg(
                    Arg::new("mint")
                        .long("mint")
                        .help("Only compare this LST mint. Repeat to compare a subset")
                        .action(ArgAction::Append)
                        .required(false),
                )
                .arg(
                    Arg::new("allow-disabled-mint")
                        .long("allow-disabled-mint")
                        .help("Allow explicitly selected disabled LST mints to be compared")
                        .action(ArgAction::SetTrue)
                        .required(false),
                )
                .arg(
                    Arg::new("poll-seconds")
                        .long("poll-seconds")
                        .help("Poll continuously, waiting this many seconds between snapshots")
                        .value_parser(clap::value_parser!(u64))
                        .required(false),
                )
                .arg(
                    Arg::new("output-file")
                        .long("output-file")
                        .help("Write the latest rendered snapshot to this file instead of stdout")
                        .required(false),
                )
                .arg(
                    Arg::new("prometheus")
                        .long("prometheus")
                        .help("Render Prometheus text exposition metrics")
                        .action(ArgAction::SetTrue)
                        .required(false),
                )
                .arg(
                    Arg::new("jupiter-api-key")
                        .long("jupiter-api-key")
                        .help("Jupiter API key. If omitted, JUPITER_API_KEY is used when set")
                        .required(false),
                )
                .arg(
                    Arg::new("jupiter-url")
                        .long("jupiter-url")
                        .help("Jupiter Swap V2 build endpoint")
                        .default_value(DEFAULT_JUPITER_BUILD_URL)
                        .required(false),
                )
                .arg(
                    Arg::new("jupiter-taker")
                        .long("jupiter-taker")
                        .help("Taker pubkey to pass to Jupiter build. Defaults to --keypair pubkey, or a generated pubkey")
                        .required(false),
                )
                .arg(
                    Arg::new("exclude-dex")
                        .long("exclude-dex")
                        .help("Jupiter DEX label to exclude. Repeatable; defaults to VaultLiquidUnstake")
                        .action(ArgAction::Append)
                        .required(false),
                )
                .arg(
                    Arg::new("jupiter-timeout-seconds")
                        .long("jupiter-timeout-seconds")
                        .help("HTTP timeout for each Jupiter request")
                        .value_parser(clap::value_parser!(u64))
                        .default_value("10")
                        .required(false),
                )
                .arg(
                    Arg::new("jupiter-request-delay-ms")
                        .long("jupiter-request-delay-ms")
                        .help("Delay between Jupiter requests. Defaults to 1100ms with an API key and 2200ms without one")
                        .value_parser(clap::value_parser!(u64))
                        .required(false),
                )
                .arg(
                    Arg::new("jupiter-retries")
                        .long("jupiter-retries")
                        .help("Retries for retryable Jupiter responses such as HTTP 429")
                        .value_parser(clap::value_parser!(u64))
                        .default_value("3")
                        .required(false),
                ),
        )
        .subcommand(
            Command::new("upsert-lst-info")
                .about("Create or update the v3 LST allowlist entry for a mint")
                .arg(string_arg("mint", "LST mint"))
                .arg(
                    Arg::new("stake-pool")
                        .long("stake-pool")
                        .help("Stake pool account. If omitted, the CLI discovers it by mint")
                        .required(false),
                )
                .arg(
                    Arg::new("disable")
                        .long("disable")
                        .help("Disable the LST instead of enabling it")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("sync-inventory")
                .about("Refresh the v3 inventory summary for active LST inventory")
                .arg(optional_u64_arg(
                    "chunk-size",
                    "Active mint entries per transaction",
                ))
                .arg(
                    Arg::new("abort")
                        .long("abort")
                        .help("Abort/clear an in-progress inventory sync")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("create-idempotent-pool-token-accounts")
                .about("Create missing pool-owned token accounts for enabled v3 LST entries")
                .arg(optional_u64_arg(
                    "chunk-size",
                    "Token accounts to create per transaction. Defaults to 8",
                )),
        )
        .subcommand(
            Command::new("inventory-status")
                .about("Report whether the v3 inventory summary is current without syncing"),
        )
        .subcommand(
            Command::new("update")
                .about("Harvest withdrawable lamports from tracked deactivating stake accounts")
                .arg(optional_u64_arg(
                    "chunk-size",
                    "Tracked stake accounts per transaction. Defaults to 8",
                ))
                .arg(
                    Arg::new("stake-account")
                        .long("stake-account")
                        .help("Tracked stake account to update. Repeat to process a subset; omit to discover all tracked stake accounts for the pool")
                        .action(ArgAction::Append)
                        .required(false),
                ),
        )
        .subcommand(
            Command::new("unstake-pool-lsts")
                .about("Authority-only: unstake pool-owned LST inventory into deactivating stake")
                .arg(string_arg("mint", "LST mint, or ALL"))
                .arg(string_arg(
                    "amount",
                    "LST token amount to unstake from pool inventory, or ALL",
                ))
                .arg(optional_u64_arg(
                    "stake-account-seed",
                    "Seed for destination stake account PDAs. Defaults to pool.total_deactivating_stake",
                )),
        )
        .subcommand(
            Command::new("unstake-pools-lsts-balanced")
                .about("Authority-only: rebalance pool-owned LST inventory down to a TVL percentage cap")
                .arg(string_arg(
                    "cap-percent",
                    "Maximum percent of total pool TVL to leave in LSTs, e.g. 10 or 10%",
                ))
                .arg(
                    Arg::new("lst-target")
                        .long("lst-target")
                        .help("Override one mint's remaining target as MINT:PERCENT of total pool TVL. Repeat for multiple mints")
                        .action(ArgAction::Append)
                        .required(false),
                )
                .arg(optional_u64_arg(
                    "stake-account-seed",
                    "Seed for destination stake account PDAs. Defaults to pool.total_deactivating_stake",
                )),
        )
        .subcommand(
            Command::new("list-pool-lsts")
                .about("List LST balances owned by the pool from configured LstInfo entries"),
        )
        .subcommand(
            Command::new("update-pool")
                .about("Authority-only: update pool config and migrate a v2 pool account to v3 size")
                .arg(pubkey_arg("manager-fee-account", "Manager fee account"))
                .arg(u32_arg("fee-max", "Maximum unstake fee in fee units"))
                .arg(u32_arg("fee-min", "Minimum unstake fee in fee units"))
                .arg(u64_arg(
                    "min-sol-for-min-fee",
                    "SOL vault lamports needed to reach minimum unstake fee",
                ))
                .arg(u8_arg("manager-fee-pct", "Percent of fees paid to manager"))
                .arg(u64_arg("vault-lamports-cap", "Pool value cap in lamports"))
                .arg(u16_arg("withdraw-sol-fee", "LP SOL withdrawal fee in fee units"))
                .arg(u16_arg(
                    "withdraw-stake-account-fee",
                    "LP stake-account withdrawal fee in fee units",
                ))
                .arg(bool_arg("flash-loans-enabled", "Whether flash loans are enabled"))
                .arg(u32_arg("flash-loan-fee", "Flash loan fee in fee units"))
                .arg(u32_arg("sell-lst-flat-fee", "sell_lst flat fee in fee units"))
                .arg(u32_arg("buy-lst-flat-fee", "buy_lst flat fee in fee units"))
                .arg(u32_arg(
                    "buy-lst-dynamic-fee-max",
                    "Maximum buy_lst dynamic fee in fee units",
                ))
                .arg(u32_arg(
                    "expected-inflation-per-epoch",
                    "Expected inflation per epoch in fee units",
                ))
                .arg(u8_arg(
                    "max-epoch-progress-pct",
                    "Maximum epoch progress percentage before trading is blocked",
                ))
                .arg(u64_arg(
                    "min-buy-lamports",
                    "Minimum total buy_lst cost in lamports. Use 0 to disable",
                ))
                .arg(u16_arg(
                    "max-rate-drift-bps",
                    "Maximum exchange-rate drift in fee units",
                ))
                .arg(pubkey_arg(
                    "maintenance-authority",
                    "Authority allowed to run pool maintenance operations",
                )),
        )
        .subcommand(
            Command::new("halt-pool")
                .about("Authority-only: halt or unhalt user-facing pool instructions")
                .arg(bool_arg("halted", "true to halt, false to unhalt")),
        )
        .subcommand(
            Command::new("create-or-update-token-metadata")
                .about("Authority-only: create or update Metaplex metadata for the pool LP mint")
                .arg(string_arg("name", "Token name"))
                .arg(string_arg("symbol", "Token symbol"))
                .arg(string_arg("uri", "Token metadata URI"))
                .arg(
                    Arg::new("token-mint")
                        .long("token-mint")
                        .help("Token mint to update. Defaults to the pool LP mint")
                        .required(false),
                ),
        )
        .subcommand(
            Command::new("list-lst-mints")
                .about("List SPL stake pools discoverable by supported stake-pool programs")
                .arg(optional_u64_arg("limit", "Maximum rows to print")),
        )
        .subcommand(
            Command::new("list-lst-info")
                .about("List LST allowlist entries configured for this pool")
                .arg(
                    Arg::new("active-only")
                        .long("active-only")
                        .help("Only show entries marked active")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(Command::new("vlp-price").about("Print the current VLP/LP token price"))
        .subcommand(Command::new("pool-info").about("Print the decoded pool account"))
        .get_matches();

    let rpc_url: &String = matches.get_one("rpc").unwrap();
    let unstake_pool_id = parse_pubkey(matches.get_one::<String>("pool").unwrap(), "pool")?;
    let simulate = matches.get_flag("simulate");
    let dump_transaction_message = matches.get_flag("dump-transaction-message");
    DUMP_TRANSACTION_MESSAGE.store(dump_transaction_message, Ordering::Relaxed);
    let new_stake_account_as_pda = !matches.get_flag("no-stake-account-as-pda");
    let wallet_keypair = load_wallet(
        matches.get_one::<String>("keypair"),
        dump_transaction_message,
    )?;
    let client_keypair = wallet_keypair
        .keypair("client setup")
        .map(|keypair| keypair.insecure_clone())
        .unwrap_or_else(|_| Keypair::new());

    let client = Client::new(
        anchor_client::Cluster::Custom(rpc_url.clone(), rpc_url.clone()),
        Rc::new(client_keypair),
    );
    let program: ProgramClient = client.program(ID_CONST)?;

    match matches.subcommand() {
        Some(("deposit", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            deposit_sol(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &pool,
                *arg_matches.get_one::<u64>("lamports").unwrap(),
                simulate,
            )
            .await?;
        }
        Some(("initialize-pool", arg_matches)) => {
            initialize_pool(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                arg_matches,
                simulate,
            )
            .await?;
        }
        Some(("withdraw", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            withdraw_sol(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &pool,
                *arg_matches.get_one::<u64>("tokens").unwrap(),
                simulate,
            )
            .await?;
        }
        Some(("withdraw-stake-account", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let stake_account_source = parse_pubkey(
                arg_matches
                    .get_one::<String>("stake-account-source")
                    .unwrap(),
                "stake-account-source",
            )?;
            let destination_keypair = read_keypair_file(
                arg_matches
                    .get_one::<String>("destination-keypair")
                    .unwrap(),
            )
            .map_err(|_| anyhow!("failed to read destination stake account keypair file"))?;
            withdraw_stake_account(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &destination_keypair,
                &pool,
                stake_account_source,
                *arg_matches.get_one::<u64>("tokens").unwrap(),
                simulate,
            )
            .await?;
        }
        Some(("unstake-stake-account", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let stake_account = parse_pubkey(
                arg_matches.get_one::<String>("stake-account").unwrap(),
                "stake-account",
            )?;
            liquid_unstake_stake_account(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &pool,
                stake_account,
                arg_matches.get_one::<u64>("min-lamports-out").copied(),
                simulate,
            )
            .await?;
        }
        Some(("quote-unstake-lst", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            let amount = *arg_matches.get_one::<u64>("amount").unwrap();
            let (_, (_, stake_pool_state)) =
                get_stake_pool_for_mint_from_supported_programs(&program.rpc(), &mint).await?;
            let quote = quote_lst_unstake(&stake_pool_state, &pool, amount)?;

            println!(
                "Quote: {} lamports received for {} {} tokens (excluding transaction fees)",
                quote, amount, mint
            );
        }
        Some(("quote-unstake-lst-wrapped", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            let amount = *arg_matches.get_one::<u64>("amount").unwrap();
            let (_, (_, stake_pool_state)) =
                get_stake_pool_for_mint_from_supported_programs(&program.rpc(), &mint).await?;
            let (quote_wsol, lamports) = quote_lst_unstake_wrapped(
                &stake_pool_state,
                &pool,
                amount,
                new_stake_account_as_pda,
            )?;

            println!(
                "Quote: {} wrapped lamports and {} native lamports received for {} {} tokens",
                quote_wsol, lamports, amount, mint
            );
        }
        Some(("unstake-lst", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            let amount = *arg_matches.get_one::<u64>("amount").unwrap();
            let min_out = arg_matches.get_one::<u64>("min-lamports-out").copied();
            let (stake_pool_program_id, _) =
                get_stake_pool_for_mint_from_supported_programs(&program.rpc(), &mint).await?;

            unstake_lst(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &stake_pool_program_id,
                &mint,
                &pool,
                amount,
                min_out,
                simulate,
                new_stake_account_as_pda,
            )
            .await?;
        }
        Some(("unstake-lst-wrapped", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            let amount = *arg_matches.get_one::<u64>("amount").unwrap();
            let min_out = arg_matches.get_one::<u64>("min-lamports-out").copied();
            let (stake_pool_program_id, _) =
                get_stake_pool_for_mint_from_supported_programs(&program.rpc(), &mint).await?;

            unstake_lst_wrapped(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &stake_pool_program_id,
                &mint,
                &pool,
                amount,
                min_out,
                simulate,
                new_stake_account_as_pda,
            )
            .await?;
        }
        Some(("quote-sell-lst", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            let amount = *arg_matches.get_one::<u64>("amount").unwrap();
            let quote =
                quote_sell_lst(&program.rpc(), &unstake_pool_id, &pool, &mint, amount).await?;
            print_sell_quote(amount, mint, &quote);
        }
        Some(("quote-buy-lst", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            let amount = *arg_matches.get_one::<u64>("amount").unwrap();
            let quote = quote_buy_lst(
                &program,
                &unstake_pool_id,
                wallet_keypair.keypair("quote-buy-lst")?,
                &pool,
                &mint,
                amount,
            )
            .await?;
            print_buy_quote(amount, mint, &quote);
        }
        Some(("compare-price", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            compare_price(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &pool,
                arg_matches,
            )
            .await?;
        }
        Some(("sell-lst", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            sell_lst(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &pool,
                &mint,
                *arg_matches.get_one::<u64>("amount").unwrap(),
                arg_matches.get_one::<u64>("min-lamports-out").copied(),
                simulate,
            )
            .await?;
        }
        Some(("buy-lst", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            buy_lst(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &pool,
                &mint,
                *arg_matches.get_one::<u64>("amount").unwrap(),
                arg_matches.get_one::<u64>("max-lamports-in").copied(),
                simulate,
            )
            .await?;
        }
        Some(("upsert-lst-info", arg_matches)) => {
            let mint = parse_pubkey(arg_matches.get_one::<String>("mint").unwrap(), "mint")?;
            let stake_pool = if let Some(stake_pool) = arg_matches.get_one::<String>("stake-pool") {
                parse_pubkey(stake_pool, "stake-pool")?
            } else {
                get_stake_pool_for_mint_from_supported_programs(&program.rpc(), &mint)
                    .await?
                    .1
                     .0
            };
            upsert_lst_info(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &mint,
                &stake_pool,
                !arg_matches.get_flag("disable"),
                simulate,
            )
            .await?;
        }
        Some(("sync-inventory", arg_matches)) => {
            sync_inventory(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                arg_matches
                    .get_one::<u64>("chunk-size")
                    .copied()
                    .unwrap_or(8) as usize,
                arg_matches.get_flag("abort"),
                simulate,
            )
            .await?;
        }
        Some(("create-idempotent-pool-token-accounts", arg_matches)) => {
            create_idempotent_pool_token_accounts(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                arg_matches
                    .get_one::<u64>("chunk-size")
                    .copied()
                    .unwrap_or(DEFAULT_UPDATE_CHUNK_SIZE as u64) as usize,
                simulate,
            )
            .await?;
        }
        Some(("inventory-status", _)) => {
            inventory_status(&program, &unstake_pool_id).await?;
        }
        Some(("update", arg_matches)) => {
            let stake_accounts = parse_optional_pubkey_values(arg_matches, "stake-account")?;
            let chunk_size = arg_matches
                .get_one::<u64>("chunk-size")
                .copied()
                .unwrap_or(DEFAULT_UPDATE_CHUNK_SIZE as u64) as usize;
            update(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                stake_accounts.as_deref(),
                chunk_size,
                simulate,
            )
            .await?;
        }
        Some(("unstake-pool-lsts", arg_matches)) => {
            unstake_pool_lsts_for_selection(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                parse_pool_lst_mint_selection(arg_matches.get_one::<String>("mint").unwrap())?,
                parse_pool_lst_amount_selection(arg_matches.get_one::<String>("amount").unwrap())?,
                arg_matches.get_one::<u64>("stake-account-seed").copied(),
                simulate,
            )
            .await?;
        }
        Some(("unstake-pools-lsts-balanced", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let cap_percent = parse_percent_units(
                arg_matches.get_one::<String>("cap-percent").unwrap(),
                "cap-percent",
            )?;
            let overrides = parse_pool_lst_target_overrides(arg_matches)?;
            unstake_pool_lsts_balanced(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                &pool,
                cap_percent,
                &overrides,
                arg_matches.get_one::<u64>("stake-account-seed").copied(),
                simulate,
            )
            .await?;
        }
        Some(("list-pool-lsts", _)) => {
            let balances = list_pool_lst_balances(&program.rpc(), &unstake_pool_id).await?;
            println!("mint,amount");
            for (mint, amount) in balances {
                println!("{mint},{amount}");
            }
        }
        Some(("update-pool", arg_matches)) => {
            update_pool(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                arg_matches,
                simulate,
            )
            .await?;
        }
        Some(("halt-pool", arg_matches)) => {
            halt_pool(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                *arg_matches.get_one::<bool>("halted").unwrap(),
                simulate,
            )
            .await?;
        }
        Some(("create-or-update-token-metadata", arg_matches)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            let token_mint = if let Some(token_mint) = arg_matches.get_one::<String>("token-mint") {
                parse_pubkey(token_mint, "token-mint")?
            } else {
                pool.lp_mint
            };
            create_or_update_token_metadata(
                &program,
                &unstake_pool_id,
                &wallet_keypair,
                token_mint,
                arg_matches.get_one::<String>("name").unwrap().clone(),
                arg_matches.get_one::<String>("symbol").unwrap().clone(),
                arg_matches.get_one::<String>("uri").unwrap().clone(),
                simulate,
            )
            .await?;
        }
        Some(("list-lst-mints", arg_matches)) => {
            let limit = *arg_matches.get_one::<u64>("limit").unwrap_or(&u64::MAX);
            let mut mints = vec![];

            for program_id in SUPPORTED_STAKE_POOL_PROGRAMS {
                mints.extend(get_stake_pool_mints(&program.rpc(), &program_id).await?);
            }

            println!("pool,program,mint");
            mints
                .into_iter()
                .take(limit as usize)
                .for_each(|(pool, program, mint)| println!("{pool},{program},{mint}"));
        }
        Some(("list-lst-info", arg_matches)) => {
            let entries = list_lst_infos(&program.rpc(), &unstake_pool_id).await?;
            println!("lst_info,mint,stake_pool,stake_pool_program,enabled,is_active,last_synced_session_id");
            for (address, entry) in entries {
                if arg_matches.get_flag("active-only") && !entry.is_active {
                    continue;
                }
                println!(
                    "{},{},{},{},{},{},{}",
                    address,
                    entry.mint,
                    entry.stake_pool,
                    entry.stake_pool_program,
                    entry.enabled,
                    entry.is_active,
                    entry.last_synced_session_id
                );
            }
        }
        Some(("vlp-price", _)) => {
            print_vlp_price(&program, &unstake_pool_id).await?;
        }
        Some(("pool-info", _)) => {
            let pool = fetch_pool(&program, unstake_pool_id).await?;
            println!("{pool:#?}");
            let inventory_summary = inventory_summary_address(&unstake_pool_id);
            if let Some(summary) =
                fetch_anchor_account::<InventorySummaryAccount>(&program.rpc(), &inventory_summary)
                    .await?
            {
                println!("InventorySummary {inventory_summary}: {summary:#?}");
            }
        }
        _ => {
            println!("No valid subcommand was provided");
        }
    };

    Ok(())
}

fn string_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name).help(help).required(true)
}

fn pubkey_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name).long(name).help(help).required(true)
}

fn pubkey_pos_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name).help(help).required(true)
}

fn u64_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .help(help)
        .required(true)
        .value_parser(clap::value_parser!(u64))
}

fn u64_pos_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .help(help)
        .required(true)
        .value_parser(clap::value_parser!(u64))
}

fn optional_u64_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .help(help)
        .required(false)
        .value_parser(clap::value_parser!(u64))
}

fn u32_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .help(help)
        .required(true)
        .value_parser(clap::value_parser!(u32))
}

fn u16_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .help(help)
        .required(true)
        .value_parser(clap::value_parser!(u16))
}

fn u8_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .help(help)
        .required(true)
        .value_parser(clap::value_parser!(u8))
}

fn bool_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .help(help)
        .required(true)
        .value_parser(clap::value_parser!(bool))
}

fn parse_pubkey(value: &str, name: &str) -> Result<Pubkey> {
    Pubkey::from_str(value).map_err(|err| anyhow!("invalid {name} pubkey {value}: {err}"))
}

fn parse_pool_lst_mint_selection(value: &str) -> Result<PoolLstMintSelection> {
    if value.eq_ignore_ascii_case("ALL") {
        return Ok(PoolLstMintSelection::All);
    }

    Ok(PoolLstMintSelection::One(parse_pubkey(value, "mint")?))
}

fn parse_pool_lst_amount_selection(value: &str) -> Result<PoolLstAmountSelection> {
    if value.eq_ignore_ascii_case("ALL") {
        return Ok(PoolLstAmountSelection::All);
    }

    let amount = value
        .parse::<u64>()
        .map_err(|err| anyhow!("invalid amount {value}: {err}"))?;
    if amount == 0 {
        return Err(anyhow!("amount must be greater than 0"));
    }
    Ok(PoolLstAmountSelection::Amount(amount))
}

fn parse_percent_units(value: &str, name: &str) -> Result<u32> {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_suffix('%').unwrap_or(trimmed).trim();
    if trimmed.is_empty() {
        return Err(anyhow!("{name} percentage cannot be empty"));
    }
    if trimmed.starts_with('-') {
        return Err(anyhow!("{name} percentage cannot be negative"));
    }

    let (whole, fraction) = trimmed
        .split_once('.')
        .map_or((trimmed, ""), |(whole, fraction)| (whole, fraction));
    if whole.is_empty() && fraction.is_empty() {
        return Err(anyhow!("invalid {name} percentage {value}"));
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!("invalid {name} percentage {value}"));
    }
    if fraction.len() > 4 {
        return Err(anyhow!(
            "{name} percentage {value} has more than 4 decimal places"
        ));
    }

    let whole_units = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u128>()
            .map_err(|err| anyhow!("invalid {name} percentage {value}: {err}"))?
            .checked_mul(PERCENT_UNITS_PER_ONE_PERCENT)
            .ok_or_else(|| anyhow!("{name} percentage {value} overflows"))?
    };
    let mut fraction_padded = fraction.to_string();
    while fraction_padded.len() < 4 {
        fraction_padded.push('0');
    }
    let fraction_units = if fraction_padded.is_empty() {
        0
    } else {
        fraction_padded
            .parse::<u128>()
            .map_err(|err| anyhow!("invalid {name} percentage {value}: {err}"))?
    };
    let units = whole_units
        .checked_add(fraction_units)
        .ok_or_else(|| anyhow!("{name} percentage {value} overflows"))?;
    if units > PERCENT_SCALE {
        return Err(anyhow!("{name} percentage cannot exceed 100%"));
    }
    Ok(units as u32)
}

fn parse_pool_lst_target_overrides(
    matches: &clap::ArgMatches,
) -> Result<Vec<PoolLstTargetOverride>> {
    let Some(values) = matches.get_many::<String>("lst-target") else {
        return Ok(Vec::new());
    };

    let mut seen = HashSet::new();
    values
        .map(|value| {
            let (mint, percent) = value
                .split_once(':')
                .or_else(|| value.split_once('='))
                .ok_or_else(|| anyhow!("invalid --lst-target {value}; expected MINT:PERCENT"))?;
            let mint = parse_pubkey(mint.trim(), "lst-target mint")?;
            if !seen.insert(mint) {
                return Err(anyhow!("duplicate --lst-target for mint {mint}"));
            }
            Ok(PoolLstTargetOverride {
                mint,
                percent: parse_percent_units(percent, "lst-target")?,
            })
        })
        .collect()
}

fn parse_optional_pubkey_values(
    matches: &clap::ArgMatches,
    name: &str,
) -> Result<Option<Vec<Pubkey>>> {
    let Some(values) = matches.get_many::<String>(name) else {
        return Ok(None);
    };
    values
        .map(|value| parse_pubkey(value, name))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn load_wallet(path: Option<&String>, allow_pubkey: bool) -> Result<Wallet> {
    if let Some(path) = path {
        match read_keypair_file(path) {
            Ok(keypair) => Ok(Wallet::Keypair(keypair)),
            Err(_) if allow_pubkey => parse_pubkey(path, "keypair").map(Wallet::Pubkey),
            Err(_) => Err(anyhow!("failed to read wallet keypair file {path}")),
        }
    } else if allow_pubkey {
        Err(anyhow!(
            "--dump-transaction-message requires --keypair with a wallet pubkey or keypair file"
        ))
    } else {
        Ok(Wallet::Keypair(Keypair::new()))
    }
}

async fn compare_price(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    pool: &PoolAccount,
    matches: &clap::ArgMatches,
) -> Result<()> {
    let amounts = parse_compare_amounts(matches)?;
    let selected_mints = parse_optional_pubkey_values(matches, "mint")?
        .map(|mints| mints.into_iter().collect::<HashSet<_>>());
    let allow_disabled_mint = matches.get_flag("allow-disabled-mint");
    let poll_seconds = matches.get_one::<u64>("poll-seconds").copied();
    if poll_seconds == Some(0) {
        return Err(anyhow!("--poll-seconds must be greater than 0"));
    }
    let output_file = matches.get_one::<String>("output-file").cloned();
    let prometheus = matches.get_flag("prometheus");
    let jupiter = build_jupiter_client(matches, wallet)?;
    let mut printed_csv_header = false;

    loop {
        let records = collect_compare_records(
            &program.rpc(),
            pool_id,
            pool,
            &jupiter,
            &amounts,
            selected_mints.as_ref(),
            allow_disabled_mint,
        )
        .await?;

        if let Some(output_file) = output_file.as_deref() {
            let rendered = if prometheus {
                render_prometheus_compare_records(pool_id, &records)
            } else {
                render_csv_compare_records(&records, true)
            };
            fs::write(output_file, rendered)
                .map_err(|err| anyhow!("failed to write compare output to {output_file}: {err}"))?;
        } else if prometheus {
            print!("{}", render_prometheus_compare_records(pool_id, &records));
            io::stdout().flush()?;
        } else {
            let include_header = !printed_csv_header;
            let rendered = render_csv_compare_records(&records, include_header);
            printed_csv_header = true;
            print!("{rendered}");
            io::stdout().flush()?;
        }

        let Some(seconds) = poll_seconds else {
            break;
        };
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }

    Ok(())
}

fn parse_compare_amounts(matches: &clap::ArgMatches) -> Result<Vec<u64>> {
    let amounts = if let Some(values) = matches.get_many::<String>("amount-sol") {
        values
            .map(|value| parse_sol_lamports(value))
            .collect::<Result<Vec<_>>>()?
    } else {
        DEFAULT_COMPARE_SOL_LAMPORTS.to_vec()
    };

    if amounts.is_empty() {
        return Err(anyhow!("at least one --amount-sol value is required"));
    }
    Ok(amounts)
}

fn parse_sol_lamports(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("SOL amount cannot be empty"));
    }
    if trimmed.starts_with('-') {
        return Err(anyhow!("SOL amount must be positive"));
    }

    let (whole, fraction) = trimmed
        .split_once('.')
        .map_or((trimmed, ""), |(whole, fraction)| (whole, fraction));
    if whole.is_empty() && fraction.is_empty() {
        return Err(anyhow!("invalid SOL amount {value}"));
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!("invalid SOL amount {value}"));
    }
    if fraction.len() > 9 {
        return Err(anyhow!("SOL amount {value} has more than 9 decimal places"));
    }

    let whole_lamports = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .map_err(|err| anyhow!("invalid SOL amount {value}: {err}"))?
            .checked_mul(LAMPORTS_PER_SOL)
            .ok_or_else(|| anyhow!("SOL amount {value} overflows u64 lamports"))?
    };
    let mut fraction_padded = fraction.to_string();
    while fraction_padded.len() < 9 {
        fraction_padded.push('0');
    }
    let fractional_lamports = if fraction_padded.is_empty() {
        0
    } else {
        fraction_padded
            .parse::<u64>()
            .map_err(|err| anyhow!("invalid SOL amount {value}: {err}"))?
    };
    let lamports = whole_lamports
        .checked_add(fractional_lamports)
        .ok_or_else(|| anyhow!("SOL amount {value} overflows u64 lamports"))?;
    if lamports == 0 {
        return Err(anyhow!("SOL amount must be greater than 0"));
    }
    Ok(lamports)
}

fn build_jupiter_client(matches: &clap::ArgMatches, wallet: &Wallet) -> Result<JupiterBuildClient> {
    let timeout_seconds = *matches.get_one::<u64>("jupiter-timeout-seconds").unwrap();
    if timeout_seconds == 0 {
        return Err(anyhow!("--jupiter-timeout-seconds must be greater than 0"));
    }
    let api_key = matches
        .get_one::<String>("jupiter-api-key")
        .cloned()
        .or_else(|| env::var("JUPITER_API_KEY").ok())
        .filter(|key| !key.trim().is_empty());
    let request_delay = matches
        .get_one::<u64>("jupiter-request-delay-ms")
        .copied()
        .map(Duration::from_millis)
        .unwrap_or_else(|| {
            if api_key.is_some() {
                Duration::from_millis(1_100)
            } else {
                Duration::from_millis(2_200)
            }
        });
    let max_retries = *matches.get_one::<u64>("jupiter-retries").unwrap();
    let excluded_dexes = matches
        .get_many::<String>("exclude-dex")
        .map(|values| {
            values
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec![DEFAULT_JUPITER_EXCLUDED_DEX.to_string()]);
    let taker = if let Some(taker) = matches.get_one::<String>("jupiter-taker") {
        parse_pubkey(taker, "jupiter-taker")?
    } else {
        wallet.pubkey()
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()?;

    Ok(JupiterBuildClient {
        http,
        base_url: matches.get_one::<String>("jupiter-url").unwrap().clone(),
        api_key,
        excluded_dexes,
        taker,
        request_delay,
        max_retries,
    })
}

async fn collect_compare_records(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    jupiter: &JupiterBuildClient,
    amounts: &[u64],
    selected_mints: Option<&HashSet<Pubkey>>,
    allow_disabled_mint: bool,
) -> Result<Vec<CompareRecord>> {
    let timestamp_unix = unix_timestamp();
    let epoch_progress = get_epoch_progress(rpc).await?;
    check_epoch_progress(rpc, pool.max_epoch_progress_pct).await?;
    check_inventory_summary_ready_for_trade(rpc, pool_id).await?;

    let mut entries = list_lst_infos(rpc, pool_id)
        .await?
        .into_iter()
        .filter(|(_, entry)| include_compare_lst_entry(entry, selected_mints, allow_disabled_mint))
        .collect_vec();
    entries.sort_by_key(|(_, entry)| entry.mint);

    if entries.is_empty() {
        return Err(if allow_disabled_mint {
            anyhow!("no enabled or explicitly allowed disabled v3 LST entries matched")
        } else {
            anyhow!("no enabled v3 LST entries matched")
        });
    }

    let mut records = Vec::new();
    for (_lst_info_address, lst_info) in entries {
        let stake_pool_state = match fetch_stake_pool_state(rpc, &lst_info.stake_pool).await {
            Ok(stake_pool_state) => stake_pool_state,
            Err(err) => {
                records.extend(error_records_for_amounts(
                    timestamp_unix,
                    *pool_id,
                    lst_info.mint,
                    amounts,
                    format!("stake-pool: {err}"),
                ));
                continue;
            }
        };

        let mint_precheck = validate_compare_mint(rpc, pool, &lst_info, &stake_pool_state).await;
        if let Err(err) = mint_precheck {
            records.extend(error_records_for_amounts(
                timestamp_unix,
                *pool_id,
                lst_info.mint,
                amounts,
                format!("v3-precheck: {err}"),
            ));
            continue;
        }

        for amount in amounts {
            records.push(
                compare_sol_to_lst(
                    timestamp_unix,
                    pool_id,
                    pool,
                    jupiter,
                    &lst_info,
                    &stake_pool_state,
                    *amount,
                    epoch_progress,
                )
                .await,
            );
            records.push(
                compare_lst_to_sol(
                    timestamp_unix,
                    pool_id,
                    pool,
                    jupiter,
                    &lst_info,
                    &stake_pool_state,
                    *amount,
                    epoch_progress,
                )
                .await,
            );
        }
    }

    Ok(records)
}

fn include_compare_lst_entry(
    entry: &LstInfoAccount,
    selected_mints: Option<&HashSet<Pubkey>>,
    allow_disabled_mint: bool,
) -> bool {
    let selected = selected_mints
        .map(|selected| selected.contains(&entry.mint))
        .unwrap_or(true);
    if !selected {
        return false;
    }

    entry.enabled || (allow_disabled_mint && selected_mints.is_some())
}

async fn validate_compare_mint(
    rpc: &RpcClient,
    pool: &PoolAccount,
    lst_info: &LstInfoAccount,
    stake_pool_state: &StakePool,
) -> Result<()> {
    if stake_pool_state.pool_mint != lst_info.mint {
        return Err(anyhow!(
            "LstInfo mint {} does not match stake pool mint {}",
            lst_info.mint,
            stake_pool_state.pool_mint
        ));
    }
    validate_stake_pool_for_v3_quote(rpc, stake_pool_state).await?;
    check_lst_rate_drift_for_info(pool, lst_info, stake_pool_state)?;
    Ok(())
}

async fn compare_sol_to_lst(
    timestamp_unix: u64,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    jupiter: &JupiterBuildClient,
    lst_info: &LstInfoAccount,
    stake_pool_state: &StakePool,
    notional_sol_lamports: u64,
    epoch_progress: u64,
) -> CompareRecord {
    let sol_mint = spl_token::native_mint::id();
    let jupiter_quote = match jupiter
        .quote(&sol_mint, &lst_info.mint, notional_sol_lamports)
        .await
    {
        Ok(quote) => quote,
        Err(err) => {
            return error_record(
                timestamp_unix,
                *pool_id,
                lst_info.mint,
                CompareDirection::SolToLst,
                notional_sol_lamports,
                format!("jupiter: {err}"),
            )
        }
    };

    if let Err(err) = reject_excluded_jupiter_route(&jupiter_quote, &jupiter.excluded_dexes) {
        return error_record(
            timestamp_unix,
            *pool_id,
            lst_info.mint,
            CompareDirection::SolToLst,
            notional_sol_lamports,
            err.to_string(),
        );
    }
    if jupiter_quote.out_amount == 0 {
        return error_record(
            timestamp_unix,
            *pool_id,
            lst_info.mint,
            CompareDirection::SolToLst,
            notional_sol_lamports,
            "jupiter returned zero LST output".to_string(),
        );
    }

    let v3_quote = match calculate_protocol_buy_quote(
        pool,
        stake_pool_state,
        jupiter_quote.out_amount,
        epoch_progress,
    ) {
        Ok(quote) => quote,
        Err(err) => {
            return error_record(
                timestamp_unix,
                *pool_id,
                lst_info.mint,
                CompareDirection::SolToLst,
                notional_sol_lamports,
                format!("v3-buy: {err}"),
            )
        }
    };

    let advantage_lamports = i128::from(jupiter_quote.in_amount) - i128::from(v3_quote.total_cost);
    CompareRecord {
        timestamp_unix,
        pool: *pool_id,
        mint: lst_info.mint,
        direction: CompareDirection::SolToLst,
        notional_sol_lamports,
        lst_amount: Some(jupiter_quote.out_amount),
        jupiter_sol_lamports: Some(jupiter_quote.in_amount),
        jupiter_lst_amount: Some(jupiter_quote.out_amount),
        v3_sol_lamports: Some(v3_quote.total_cost),
        v3_lst_amount: Some(jupiter_quote.out_amount),
        v3_advantage_lamports: Some(advantage_lamports),
        v3_advantage_bps: advantage_bps(advantage_lamports, jupiter_quote.in_amount),
        jupiter_route: jupiter_quote.route_labels,
        error: None,
    }
}

async fn compare_lst_to_sol(
    timestamp_unix: u64,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    jupiter: &JupiterBuildClient,
    lst_info: &LstInfoAccount,
    stake_pool_state: &StakePool,
    notional_sol_lamports: u64,
    epoch_progress: u64,
) -> CompareRecord {
    let lst_amount = match calculate_lst_amount_for_sol_value(
        notional_sol_lamports,
        stake_pool_state,
        pool.expected_inflation_per_epoch,
        epoch_progress,
    ) {
        Ok(amount) if amount > 0 => amount,
        Ok(_) => {
            return error_record(
                timestamp_unix,
                *pool_id,
                lst_info.mint,
                CompareDirection::LstToSol,
                notional_sol_lamports,
                "notional SOL amount converts to zero LST tokens".to_string(),
            )
        }
        Err(err) => {
            return error_record(
                timestamp_unix,
                *pool_id,
                lst_info.mint,
                CompareDirection::LstToSol,
                notional_sol_lamports,
                format!("lst-amount: {err}"),
            )
        }
    };

    let sol_mint = spl_token::native_mint::id();
    let jupiter_quote = match jupiter.quote(&lst_info.mint, &sol_mint, lst_amount).await {
        Ok(quote) => quote,
        Err(err) => {
            return error_record(
                timestamp_unix,
                *pool_id,
                lst_info.mint,
                CompareDirection::LstToSol,
                notional_sol_lamports,
                format!("jupiter: {err}"),
            )
        }
    };
    if let Err(err) = reject_excluded_jupiter_route(&jupiter_quote, &jupiter.excluded_dexes) {
        return error_record(
            timestamp_unix,
            *pool_id,
            lst_info.mint,
            CompareDirection::LstToSol,
            notional_sol_lamports,
            err.to_string(),
        );
    }

    let v3_quote = match calculate_sell_quote(pool, stake_pool_state, lst_amount, epoch_progress) {
        Ok(quote) => quote,
        Err(err) => {
            return error_record(
                timestamp_unix,
                *pool_id,
                lst_info.mint,
                CompareDirection::LstToSol,
                notional_sol_lamports,
                format!("v3-sell: {err}"),
            )
        }
    };
    let advantage_lamports =
        i128::from(v3_quote.amount_to_user) - i128::from(jupiter_quote.out_amount);

    CompareRecord {
        timestamp_unix,
        pool: *pool_id,
        mint: lst_info.mint,
        direction: CompareDirection::LstToSol,
        notional_sol_lamports,
        lst_amount: Some(lst_amount),
        jupiter_sol_lamports: Some(jupiter_quote.out_amount),
        jupiter_lst_amount: Some(jupiter_quote.in_amount),
        v3_sol_lamports: Some(v3_quote.amount_to_user),
        v3_lst_amount: Some(lst_amount),
        v3_advantage_lamports: Some(advantage_lamports),
        v3_advantage_bps: advantage_bps(advantage_lamports, jupiter_quote.out_amount),
        jupiter_route: jupiter_quote.route_labels,
        error: None,
    }
}

impl JupiterBuildClient {
    async fn quote(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
    ) -> Result<JupiterQuote> {
        let mut params = vec![
            ("inputMint", input_mint.to_string()),
            ("outputMint", output_mint.to_string()),
            ("amount", amount.to_string()),
            ("taker", self.taker.to_string()),
        ];
        if !self.excluded_dexes.is_empty() {
            params.push(("excludeDexes", self.excluded_dexes.join(",")));
        }

        let response = self.send_quote_request(&params).await?;
        let response = response
            .json::<JupiterBuildQuoteResponse>()
            .await
            .map_err(|err| anyhow!("failed to decode Jupiter build response: {err}"))?;
        if self.request_delay > Duration::ZERO {
            tokio::time::sleep(self.request_delay).await;
        }

        Ok(JupiterQuote {
            in_amount: parse_jupiter_u64(&response.in_amount, "inAmount")?,
            out_amount: parse_jupiter_u64(&response.out_amount, "outAmount")?,
            route_labels: response
                .route_plan
                .into_iter()
                .map(|step| step.swap_info.label)
                .collect(),
        })
    }

    async fn send_quote_request(&self, params: &[(&str, String)]) -> Result<reqwest::Response> {
        for attempt in 0..=self.max_retries {
            let mut request = self.http.get(&self.base_url).query(params);
            if let Some(api_key) = self.api_key.as_ref() {
                request = request.header("x-api-key", api_key);
            }
            let response = request
                .send()
                .await
                .map_err(|err| anyhow!("Jupiter build request failed: {err}"))?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status.as_u16() == 429 && attempt < self.max_retries {
                let delay = retry_after_delay(response.headers(), self.request_delay);
                tokio::time::sleep(delay).await;
                continue;
            }
            let body = response
                .text()
                .await
                .unwrap_or_else(|err| format!("failed to read response body: {err}"));
            return Err(anyhow!("Jupiter build returned {status}: {body}"));
        }

        Err(anyhow!("Jupiter build retries exhausted"))
    }
}

fn parse_jupiter_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|err| anyhow!("invalid Jupiter {name} {value}: {err}"))
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap, fallback: Duration) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(fallback.max(Duration::from_millis(1_000)))
}

fn reject_excluded_jupiter_route(quote: &JupiterQuote, excluded_dexes: &[String]) -> Result<()> {
    for label in &quote.route_labels {
        if excluded_dexes
            .iter()
            .any(|excluded| label.eq_ignore_ascii_case(excluded))
        {
            return Err(anyhow!(
                "Jupiter route still contains excluded DEX label {label}"
            ));
        }
    }
    Ok(())
}

async fn fetch_stake_pool_state(rpc: &RpcClient, stake_pool: &Pubkey) -> Result<StakePool> {
    let account = rpc
        .get_account(stake_pool)
        .await
        .map_err(|err| anyhow!("failed to fetch stake pool {stake_pool}: {err}"))?;
    let mut data = account.data.as_slice();
    StakePool::deserialize(&mut data)
        .map_err(|err| anyhow!("failed to deserialize stake pool {stake_pool}: {err}"))
}

fn calculate_lst_amount_for_sol_value(
    sol_lamports: u64,
    stake_pool_state: &StakePool,
    expected_inflation_per_epoch: u32,
    epoch_progress: u64,
) -> Result<u64> {
    calculate_lst_amount_for_sol_value_parts(
        sol_lamports,
        stake_pool_state.total_lamports,
        stake_pool_state.pool_token_supply,
        expected_inflation_per_epoch,
        epoch_progress,
    )
}

fn calculate_lst_amount_for_sol_value_parts(
    sol_lamports: u64,
    total_lamports: u64,
    pool_token_supply: u64,
    expected_inflation_per_epoch: u32,
    epoch_progress: u64,
) -> Result<u64> {
    if total_lamports == 0 {
        return Err(anyhow!("stake pool total lamports is zero"));
    }
    if pool_token_supply == 0 {
        return Err(anyhow!("stake pool token supply is zero"));
    }
    let multiplier = calculate_inflation_multiplier(epoch_progress, expected_inflation_per_epoch)?;
    Ok(u64::try_from(
        u128::from(sol_lamports)
            .checked_mul(u128::from(pool_token_supply))
            .ok_or_else(|| anyhow!("LST amount overflow"))?
            .checked_mul(u128::from(INFLATION_PCT_DIVISOR))
            .ok_or_else(|| anyhow!("LST amount overflow"))?
            .checked_div(u128::from(total_lamports))
            .ok_or_else(|| anyhow!("LST amount underflow"))?
            .checked_div(u128::from(multiplier))
            .ok_or_else(|| anyhow!("LST amount underflow"))?,
    )
    .map_err(|_| anyhow!("LST amount overflow"))?)
}

fn error_records_for_amounts(
    timestamp_unix: u64,
    pool: Pubkey,
    mint: Pubkey,
    amounts: &[u64],
    error: String,
) -> Vec<CompareRecord> {
    amounts
        .iter()
        .flat_map(|amount| {
            [
                error_record(
                    timestamp_unix,
                    pool,
                    mint,
                    CompareDirection::SolToLst,
                    *amount,
                    error.clone(),
                ),
                error_record(
                    timestamp_unix,
                    pool,
                    mint,
                    CompareDirection::LstToSol,
                    *amount,
                    error.clone(),
                ),
            ]
        })
        .collect()
}

fn error_record(
    timestamp_unix: u64,
    pool: Pubkey,
    mint: Pubkey,
    direction: CompareDirection,
    notional_sol_lamports: u64,
    error: String,
) -> CompareRecord {
    CompareRecord {
        timestamp_unix,
        pool,
        mint,
        direction,
        notional_sol_lamports,
        lst_amount: None,
        jupiter_sol_lamports: None,
        jupiter_lst_amount: None,
        v3_sol_lamports: None,
        v3_lst_amount: None,
        v3_advantage_lamports: None,
        v3_advantage_bps: None,
        jupiter_route: Vec::new(),
        error: Some(error),
    }
}

fn advantage_bps(advantage_lamports: i128, baseline_lamports: u64) -> Option<f64> {
    if baseline_lamports == 0 {
        return None;
    }
    Some((advantage_lamports as f64) * 10_000.0 / (baseline_lamports as f64))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn render_csv_compare_records(records: &[CompareRecord], include_header: bool) -> String {
    let mut output = String::new();
    if include_header {
        output.push_str(csv_compare_header());
    }
    for record in records {
        output.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            record.timestamp_unix,
            record.pool,
            record.mint,
            record.direction.label(),
            format_lamports_as_sol(record.notional_sol_lamports as u128),
            record.notional_sol_lamports,
            optional_u64_csv(record.lst_amount),
            optional_u64_csv(record.jupiter_sol_lamports),
            optional_u64_csv(record.jupiter_lst_amount),
            optional_u64_csv(record.v3_sol_lamports),
            optional_u64_csv(record.v3_lst_amount),
            optional_i128_csv(record.v3_advantage_lamports),
            optional_f64_csv(record.v3_advantage_bps),
            csv_escape(&record.jupiter_route.join(">")),
            if record.success() { "ok" } else { "error" },
            csv_escape(record.error.as_deref().unwrap_or("")),
            optional_bool_csv(unstake_pool_better(record)),
        ));
    }
    output
}

fn csv_compare_header() -> &'static str {
    "timestamp,pool,mint,direction,notional_sol,notional_sol_lamports,lst_amount,jupiter_sol_lamports,jupiter_lst_amount,v3_sol_lamports,v3_lst_amount,v3_advantage_lamports,v3_advantage_bps,jupiter_route,status,error,unstake_pool_better\n"
}

fn render_prometheus_compare_records(pool_id: &Pubkey, records: &[CompareRecord]) -> String {
    let mut output = String::new();
    output.push_str("# HELP liquid_unstaker_compare_quote_success 1 when a compare quote row succeeded, 0 when it failed.\n");
    output.push_str("# TYPE liquid_unstaker_compare_quote_success gauge\n");
    output.push_str("# HELP liquid_unstaker_compare_v3_advantage_lamports Positive when the v3 pool is better than Jupiter excluding the liquid unstaker route.\n");
    output.push_str("# TYPE liquid_unstaker_compare_v3_advantage_lamports gauge\n");
    output.push_str("# HELP liquid_unstaker_compare_v3_advantage_bps Positive basis-point advantage when the v3 pool is better than Jupiter excluding the liquid unstaker route.\n");
    output.push_str("# TYPE liquid_unstaker_compare_v3_advantage_bps gauge\n");
    output.push_str("# HELP liquid_unstaker_compare_jupiter_sol_lamports Jupiter SOL-side quote amount in lamports.\n");
    output.push_str("# TYPE liquid_unstaker_compare_jupiter_sol_lamports gauge\n");
    output.push_str(
        "# HELP liquid_unstaker_compare_v3_sol_lamports V3 SOL-side quote amount in lamports.\n",
    );
    output.push_str("# TYPE liquid_unstaker_compare_v3_sol_lamports gauge\n");
    output.push_str(
        "# HELP liquid_unstaker_compare_lst_amount LST token amount used for the comparison row.\n",
    );
    output.push_str("# TYPE liquid_unstaker_compare_lst_amount gauge\n");
    output.push_str("# HELP liquid_unstaker_compare_last_run_unixtime Unix timestamp of the latest rendered comparison snapshot.\n");
    output.push_str("# TYPE liquid_unstaker_compare_last_run_unixtime gauge\n");

    let latest_timestamp = records
        .iter()
        .map(|record| record.timestamp_unix)
        .max()
        .unwrap_or_else(unix_timestamp);
    output.push_str(&format!(
        "liquid_unstaker_compare_last_run_unixtime{{pool=\"{}\"}} {}\n",
        prom_escape_label(&pool_id.to_string()),
        latest_timestamp
    ));

    for record in records {
        let labels = prometheus_record_labels(record);
        output.push_str(&format!(
            "liquid_unstaker_compare_quote_success{{{labels}}} {}\n",
            if record.success() { 1 } else { 0 }
        ));
        if let Some(value) = record.v3_advantage_lamports {
            output.push_str(&format!(
                "liquid_unstaker_compare_v3_advantage_lamports{{{labels}}} {value}\n",
            ));
        }
        if let Some(value) = record.v3_advantage_bps {
            output.push_str(&format!(
                "liquid_unstaker_compare_v3_advantage_bps{{{labels}}} {value:.9}\n",
            ));
        }
        if let Some(value) = record.jupiter_sol_lamports {
            output.push_str(&format!(
                "liquid_unstaker_compare_jupiter_sol_lamports{{{labels}}} {value}\n",
            ));
        }
        if let Some(value) = record.v3_sol_lamports {
            output.push_str(&format!(
                "liquid_unstaker_compare_v3_sol_lamports{{{labels}}} {value}\n",
            ));
        }
        if let Some(value) = record.lst_amount {
            output.push_str(&format!(
                "liquid_unstaker_compare_lst_amount{{{labels}}} {value}\n",
            ));
        }
    }

    output
}

fn prometheus_record_labels(record: &CompareRecord) -> String {
    format!(
        "pool=\"{}\",mint=\"{}\",direction=\"{}\",notional_lamports=\"{}\",notional_sol=\"{}\"",
        prom_escape_label(&record.pool.to_string()),
        prom_escape_label(&record.mint.to_string()),
        record.direction.label(),
        record.notional_sol_lamports,
        format_lamports_as_sol(record.notional_sol_lamports as u128),
    )
}

fn optional_u64_csv(value: Option<u64>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn optional_i128_csv(value: Option<i128>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn optional_f64_csv(value: Option<f64>) -> String {
    value.map(|value| format!("{value:.9}")).unwrap_or_default()
}

fn optional_bool_csv(value: Option<bool>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn unstake_pool_better(record: &CompareRecord) -> Option<bool> {
    record.v3_advantage_lamports.map(|advantage| advantage > 0)
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn prom_escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

async fn fetch_pool(program: &ProgramClient, pool: Pubkey) -> Result<PoolAccount> {
    program
        .account::<PoolAccount>(pool)
        .await
        .map_err(|err| anyhow!("failed to fetch v3 pool {pool}: {err}"))
}

fn inventory_summary_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"inventory_summary", pool.as_ref()], &ID_CONST).0
}

fn inventory_sync_state_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"inventory_sync", pool.as_ref()], &ID_CONST).0
}

fn pool_address(authority: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool", authority.as_ref()], &ID_CONST).0
}

fn sol_vault_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"sol_vault", pool.as_ref()], &ID_CONST).0
}

fn lp_mint_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lp_mint", pool.as_ref()], &ID_CONST).0
}

fn lst_info_address(pool: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lst_info", pool.as_ref(), mint.as_ref()], &ID_CONST).0
}

fn stake_account_info_address(stake_account: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"stake_account_info", stake_account.as_ref()], &ID_CONST).0
}

fn token_metadata_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"metadata",
            MPL_TOKEN_METADATA_PROGRAM.as_ref(),
            mint.as_ref(),
        ],
        &MPL_TOKEN_METADATA_PROGRAM,
    )
    .0
}

fn create_ata_idempotent_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    associated_token::spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        payer,
        owner,
        mint,
        &spl_token::id(),
    )
}

fn token_account_address(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    associated_token::get_associated_token_address(owner, mint)
}

async fn fetch_anchor_account<T: AccountDeserialize>(
    rpc: &RpcClient,
    address: &Pubkey,
) -> Result<Option<T>> {
    let Some(account) = fetch_optional_account(rpc, address).await? else {
        return Ok(None);
    };
    let mut data = account.data.as_slice();
    Ok(Some(T::try_deserialize(&mut data)?))
}

async fn get_token_account_amount(rpc: &RpcClient, address: &Pubkey) -> Result<Option<u64>> {
    let Some(account) = fetch_optional_account(rpc, address).await? else {
        return Ok(None);
    };
    let token_account = spl_token::state::Account::unpack(&account.data)?;
    Ok(Some(token_account.amount))
}

async fn fetch_optional_account(
    rpc: &RpcClient,
    address: &Pubkey,
) -> Result<Option<solana_sdk::account::Account>> {
    let accounts = rpc.get_multiple_accounts(&[*address]).await?;
    Ok(accounts.into_iter().next().flatten())
}

fn token_amount_from_account(account: &solana_sdk::account::Account) -> Option<u64> {
    if account.owner != spl_token::id() {
        return None;
    }
    spl_token::state::Account::unpack(&account.data)
        .ok()
        .map(|account| account.amount)
}

fn token_amount_from_ui_account(account: &UiAccount) -> Option<u64> {
    let account = account.decode::<solana_sdk::account::Account>()?;
    token_amount_from_account(&account)
}

async fn deposit_sol(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    pool: &PoolAccount,
    lamports: u64,
    simulate: bool,
) -> Result<()> {
    let user_lp_account = token_account_address(&wallet.pubkey(), &pool.lp_mint);
    let inventory_summary = inventory_summary_address(pool_id);

    let mut instructions = vec![create_ata_idempotent_ix(
        &wallet.pubkey(),
        &wallet.pubkey(),
        &pool.lp_mint,
    )];
    instructions.extend(
        program
            .request()
            .accounts(lu_client::accounts::DepositSol {
                pool: *pool_id,
                sol_vault: pool.sol_vault,
                lp_mint: pool.lp_mint,
                user: wallet.pubkey(),
                user_lp_account,
                inventory_summary,
                clock: solana_sdk::sysvar::clock::id(),
                system_program: solana_sdk::system_program::id(),
                token_program: spl_token::id(),
            })
            .args(lu_client::args::DepositSol { amount: lamports })
            .instructions()?,
    );

    send_instructions(program, wallet, instructions, &[], simulate, None).await
}

async fn withdraw_sol(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    pool: &PoolAccount,
    lp_tokens: u64,
    simulate: bool,
) -> Result<()> {
    let user_lp_account = token_account_address(&wallet.pubkey(), &pool.lp_mint);
    let inventory_summary = inventory_summary_address(pool_id);

    let instructions = program
        .request()
        .accounts(lu_client::accounts::WithdrawSol {
            pool: *pool_id,
            sol_vault: pool.sol_vault,
            lp_mint: pool.lp_mint,
            user: wallet.pubkey(),
            user_lp_account,
            inventory_summary,
            clock: solana_sdk::sysvar::clock::id(),
            system_program: solana_sdk::system_program::id(),
            token_program: spl_token::id(),
        })
        .args(lu_client::args::WithdrawSol { lp_tokens })
        .instructions()?;

    send_instructions(program, wallet, instructions, &[], simulate, None).await
}

async fn initialize_pool(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    args: &clap::ArgMatches,
    simulate: bool,
) -> Result<()> {
    let expected_pool = pool_address(&wallet.pubkey());
    if expected_pool != *pool_id {
        return Err(anyhow!(
            "--pool must be the pool PDA for the authority {}: expected {}",
            wallet.pubkey(),
            expected_pool
        ));
    }

    let manager_fee_account = parse_pubkey(
        args.get_one::<String>("manager-fee-account").unwrap(),
        "manager-fee-account",
    )?;
    let sol_vault = sol_vault_address(pool_id);
    let lp_mint = lp_mint_address(pool_id);
    let instructions = program
        .request()
        .accounts(lu_client::accounts::InitializePool {
            pool: *pool_id,
            authority: wallet.pubkey(),
            sol_vault,
            lp_mint,
            manager_fee_account,
            system_program: solana_sdk::system_program::id(),
            token_program: spl_token::id(),
            rent: solana_sdk::sysvar::rent::id(),
        })
        .args(lu_client::args::InitializePool {
            fee_max: *args.get_one::<u32>("fee-max").unwrap(),
            fee_min: *args.get_one::<u32>("fee-min").unwrap(),
            min_sol_for_min_fee: *args.get_one::<u64>("min-sol-for-min-fee").unwrap(),
            manager_fee_pct: *args.get_one::<u8>("manager-fee-pct").unwrap(),
            vault_lamports_cap: *args.get_one::<u64>("vault-lamports-cap").unwrap(),
            withdraw_sol_fee: *args.get_one::<u16>("withdraw-sol-fee").unwrap(),
            withdraw_stake_account_fee: *args.get_one::<u16>("withdraw-stake-account-fee").unwrap(),
            flash_loans_enabled: *args.get_one::<bool>("flash-loans-enabled").unwrap(),
            flash_loan_fee: *args.get_one::<u32>("flash-loan-fee").unwrap(),
            min_buy_lamports: *args.get_one::<u64>("min-buy-lamports").unwrap(),
        })
        .instructions()?;

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![*pool_id, sol_vault, lp_mint]),
    )
    .await
}

async fn withdraw_stake_account(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    destination_stake_account: &Keypair,
    pool: &PoolAccount,
    stake_account_source: Pubkey,
    lp_tokens: u64,
    simulate: bool,
) -> Result<()> {
    let user_lp_account = token_account_address(&wallet.pubkey(), &pool.lp_mint);
    let stake_account_info_source = stake_account_info_address(&stake_account_source);
    let inventory_summary = inventory_summary_address(pool_id);
    let instructions = program
        .request()
        .accounts(lu_client::accounts::WithdrawStakeAccount {
            pool: *pool_id,
            sol_vault: pool.sol_vault,
            lp_mint: pool.lp_mint,
            user: wallet.pubkey(),
            user_lp_account,
            stake_account_destination: destination_stake_account.pubkey(),
            stake_account_source,
            stake_account_info_source,
            system_program: solana_sdk::system_program::id(),
            token_program: spl_token::id(),
            stake_program: solana_sdk::stake::program::id(),
            clock: solana_sdk::sysvar::clock::id(),
            inventory_summary,
        })
        .args(lu_client::args::WithdrawStakeAccount { lp_tokens })
        .instructions()?;

    send_instructions(
        program,
        wallet,
        instructions,
        &[destination_stake_account],
        simulate,
        Some(vec![
            wallet.pubkey(),
            destination_stake_account.pubkey(),
            stake_account_source,
            pool.sol_vault,
            *pool_id,
        ]),
    )
    .await
}

async fn liquid_unstake_stake_account(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    pool: &PoolAccount,
    stake_account: Pubkey,
    minimum_lamports_out: Option<u64>,
    simulate: bool,
) -> Result<()> {
    let stake_account_info = stake_account_info_address(&stake_account);
    let instructions = program
        .request()
        .accounts(lu_client::accounts::LiquidUnstakeStakeAccount {
            pool: *pool_id,
            user: wallet.pubkey(),
            stake_account,
            stake_account_info,
            sol_vault: pool.sol_vault,
            user_sol_account: wallet.pubkey(),
            manager_fee_account: pool.manager_fee_account,
            stake_program: solana_sdk::stake::program::id(),
            token_program: spl_token::id(),
            system_program: solana_sdk::system_program::id(),
            clock: solana_sdk::sysvar::clock::id(),
        })
        .args(lu_client::args::LiquidUnstakeStakeAccount {
            minimum_lamports_out,
        })
        .instructions()?;

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![
            wallet.pubkey(),
            stake_account,
            stake_account_info,
            pool.sol_vault,
            pool.manager_fee_account,
        ]),
    )
    .await
}

async fn sell_lst(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    pool: &PoolAccount,
    mint: &Pubkey,
    amount: u64,
    minimum_lamports_out: Option<u64>,
    simulate: bool,
) -> Result<()> {
    let (_, (stake_pool_address, _stake_pool_state)) =
        get_stake_pool_for_mint_from_supported_programs(&program.rpc(), mint).await?;
    let pool_lst_token_account = token_account_address(pool_id, mint);
    let user_lst_account = token_account_address(&wallet.pubkey(), mint);
    let user_wsol_account = token_account_address(&wallet.pubkey(), &spl_token::native_mint::id());
    let lst_info = lst_info_address(pool_id, mint);
    let inventory_summary = inventory_summary_address(pool_id);

    let mut instructions = vec![
        create_ata_idempotent_ix(&wallet.pubkey(), pool_id, mint),
        create_ata_idempotent_ix(&wallet.pubkey(), &wallet.pubkey(), mint),
        create_ata_idempotent_ix(
            &wallet.pubkey(),
            &wallet.pubkey(),
            &spl_token::native_mint::id(),
        ),
    ];
    instructions.extend(
        program
            .request()
            .accounts(lu_client::accounts::SellLst {
                pool: *pool_id,
                pool_lst_token_account,
                user_transfer_authority: wallet.pubkey(),
                user_lst_account,
                user_wsol_account,
                lst_mint: *mint,
                stake_pool: stake_pool_address,
                lst_info,
                sol_vault: pool.sol_vault,
                manager_fee_account: pool.manager_fee_account,
                inventory_summary,
                token_program: spl_token::id(),
                system_program: solana_sdk::system_program::id(),
            })
            .args(lu_client::args::SellLst {
                lst_amount: amount,
                minimum_lamports_out,
            })
            .instructions()?,
    );

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![
            wallet.pubkey(),
            user_wsol_account,
            pool_lst_token_account,
            pool.sol_vault,
            pool.manager_fee_account,
        ]),
    )
    .await
}

struct BuyLstInstructionBundle {
    instructions: Vec<Instruction>,
    pool_lst_token_account: Pubkey,
    user_wsol_account: Pubkey,
    user_lst_account: Pubkey,
    wsol_buffer_account: Pubkey,
}

fn buy_lst_instructions(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Pubkey,
    pool: &PoolAccount,
    mint: &Pubkey,
    stake_pool_address: &Pubkey,
    amount: u64,
    maximum_lamports_in: Option<u64>,
) -> Result<BuyLstInstructionBundle> {
    let pool_lst_token_account = token_account_address(pool_id, mint);
    let user_wsol_account = token_account_address(wallet, &spl_token::native_mint::id());
    let user_lst_account = token_account_address(wallet, mint);
    let wsol_buffer_account = token_account_address(pool_id, &spl_token::native_mint::id());
    let lst_info = lst_info_address(pool_id, mint);
    let inventory_summary = inventory_summary_address(pool_id);

    let mut instructions = vec![
        create_ata_idempotent_ix(wallet, pool_id, mint),
        create_ata_idempotent_ix(wallet, wallet, &spl_token::native_mint::id()),
        create_ata_idempotent_ix(wallet, wallet, mint),
    ];
    instructions.extend(
        program
            .request()
            .accounts(lu_client::accounts::BuyLst {
                pool: *pool_id,
                pool_lst_token_account,
                user: *wallet,
                user_wsol_account,
                user_lst_account,
                lst_mint: *mint,
                native_mint: spl_token::native_mint::id(),
                stake_pool: *stake_pool_address,
                lst_info,
                sol_vault: pool.sol_vault,
                manager_fee_account: pool.manager_fee_account,
                wsol_buffer_account,
                inventory_summary,
                token_program: spl_token::id(),
                associated_token_program: associated_token::spl_associated_token_account::id(),
                system_program: solana_sdk::system_program::id(),
                clock: solana_sdk::sysvar::clock::id(),
            })
            .args(lu_client::args::BuyLst {
                lst_amount: amount,
                maximum_lamports_in,
            })
            .instructions()?,
    );

    Ok(BuyLstInstructionBundle {
        instructions,
        pool_lst_token_account,
        user_wsol_account,
        user_lst_account,
        wsol_buffer_account,
    })
}

async fn buy_lst(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    pool: &PoolAccount,
    mint: &Pubkey,
    amount: u64,
    maximum_lamports_in: Option<u64>,
    simulate: bool,
) -> Result<()> {
    let (_, (stake_pool_address, _stake_pool_state)) =
        get_stake_pool_for_mint_from_supported_programs(&program.rpc(), mint).await?;
    let buy_instructions = buy_lst_instructions(
        program,
        pool_id,
        &wallet.pubkey(),
        pool,
        mint,
        &stake_pool_address,
        amount,
        maximum_lamports_in,
    )?;

    let simulation_accounts = vec![
        wallet.pubkey(),
        buy_instructions.user_wsol_account,
        buy_instructions.user_lst_account,
        buy_instructions.pool_lst_token_account,
        pool.sol_vault,
        pool.manager_fee_account,
        buy_instructions.wsol_buffer_account,
    ];

    send_instructions(
        program,
        wallet,
        buy_instructions.instructions,
        &[],
        simulate,
        Some(simulation_accounts),
    )
    .await
}

async fn upsert_lst_info(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    mint: &Pubkey,
    stake_pool: &Pubkey,
    enabled: bool,
    simulate: bool,
) -> Result<()> {
    let lst_info = lst_info_address(pool_id, mint);
    let instructions = program
        .request()
        .accounts(lu_client::accounts::UpsertLstInfo {
            pool: *pool_id,
            authority: wallet.pubkey(),
            lst_mint: *mint,
            stake_pool: *stake_pool,
            lst_info,
            system_program: solana_sdk::system_program::id(),
        })
        .args(lu_client::args::UpsertLstInfo { enabled })
        .instructions()?;

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![lst_info]),
    )
    .await
}

struct PoolTokenAccountEntry {
    lst_info: Pubkey,
    mint: Pubkey,
    pool_lst_token_account: Pubkey,
    is_active: bool,
}

async fn create_idempotent_pool_token_accounts(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    chunk_size: usize,
    simulate: bool,
) -> Result<()> {
    if chunk_size == 0 {
        return Err(anyhow!("chunk-size must be greater than 0"));
    }

    let enabled_entries = list_lst_infos(&program.rpc(), pool_id)
        .await?
        .into_iter()
        .filter(|(_, lst_info)| lst_info.enabled)
        .map(|(lst_info_address, lst_info)| PoolTokenAccountEntry {
            lst_info: lst_info_address,
            mint: lst_info.mint,
            pool_lst_token_account: token_account_address(pool_id, &lst_info.mint),
            is_active: lst_info.is_active,
        })
        .collect_vec();

    println!(
        "Checking {} enabled v3 LST entries for pool-owned token accounts",
        enabled_entries.len()
    );

    if enabled_entries.is_empty() {
        println!("No enabled v3 LST entries found");
        return Ok(());
    }

    let mut existing_count = 0usize;
    let mut missing_entries = Vec::new();

    for chunk in enabled_entries.chunks(100) {
        let token_accounts = chunk
            .iter()
            .map(|entry| entry.pool_lst_token_account)
            .collect_vec();
        let accounts = program.rpc().get_multiple_accounts(&token_accounts).await?;

        for (entry, account) in chunk.iter().zip(accounts) {
            if let Some(account) = account {
                validate_pool_token_account(pool_id, entry, &account)?;
                existing_count += 1;
            } else {
                missing_entries.push(PoolTokenAccountEntry {
                    lst_info: entry.lst_info,
                    mint: entry.mint,
                    pool_lst_token_account: entry.pool_lst_token_account,
                    is_active: entry.is_active,
                });
            }
        }
    }

    println!("  existing_pool_token_accounts={existing_count}");
    println!("  missing_pool_token_accounts={}", missing_entries.len());

    if missing_entries.is_empty() {
        println!("All enabled v3 LST pool token accounts already exist");
        return Ok(());
    }

    println!("mint,pool_token_account,lst_info,is_active");
    for entry in &missing_entries {
        println!(
            "{},{},{},{}",
            entry.mint, entry.pool_lst_token_account, entry.lst_info, entry.is_active
        );
    }

    let chunk_count = missing_entries.len().div_ceil(chunk_size);
    for (chunk_index, chunk) in missing_entries.chunks(chunk_size).enumerate() {
        println!(
            "Creating {} missing pool token accounts in transaction {}/{}",
            chunk.len(),
            chunk_index + 1,
            chunk_count
        );
        let instructions = chunk
            .iter()
            .map(|entry| create_ata_idempotent_ix(&wallet.pubkey(), pool_id, &entry.mint))
            .collect_vec();
        let simulation_accounts = chunk
            .iter()
            .map(|entry| entry.pool_lst_token_account)
            .collect_vec();

        send_instructions(
            program,
            wallet,
            instructions,
            &[],
            simulate,
            Some(simulation_accounts),
        )
        .await?;
    }

    Ok(())
}

fn validate_pool_token_account(
    pool_id: &Pubkey,
    entry: &PoolTokenAccountEntry,
    account: &solana_sdk::account::Account,
) -> Result<()> {
    if account.owner != spl_token::id() {
        return Err(anyhow!(
            "pool token account {} for mint {} exists but is owned by {} instead of {}",
            entry.pool_lst_token_account,
            entry.mint,
            account.owner,
            spl_token::id()
        ));
    }

    let token_account = spl_token::state::Account::unpack(&account.data)?;
    if token_account.owner != *pool_id {
        return Err(anyhow!(
            "pool token account {} for mint {} exists but has token owner {} instead of {}",
            entry.pool_lst_token_account,
            entry.mint,
            token_account.owner,
            pool_id
        ));
    }
    if token_account.mint != entry.mint {
        return Err(anyhow!(
            "pool token account {} exists but has mint {} instead of {}",
            entry.pool_lst_token_account,
            token_account.mint,
            entry.mint
        ));
    }

    Ok(())
}

async fn sync_inventory(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    chunk_size: usize,
    abort: bool,
    simulate: bool,
) -> Result<()> {
    if chunk_size == 0 {
        return Err(anyhow!("chunk-size must be greater than 0"));
    }

    let inventory_summary = inventory_summary_address(pool_id);
    let inventory_sync_state = inventory_sync_state_address(pool_id);
    let mut chunks = if abort {
        println!("Aborting/clearing inventory sync session; no LSTs will be synced");
        vec![vec![]]
    } else {
        let pool = fetch_pool(program, *pool_id).await?;
        let lst_info_records = list_lst_info_records(&program.rpc(), pool_id).await?;
        print_lst_info_records(&lst_info_records);
        let entries = active_inventory_entries_from_records(pool_id, lst_info_records);

        if let Some(reason) =
            inventory_sync_needed(&program.rpc(), pool_id, &pool, &inventory_summary, &entries)
                .await?
        {
            println!("Inventory sync needed: {reason}");
        } else {
            println!(
                "Inventory summary is already current for {} active v3 LST inventory entries; no sync transaction sent",
                entries.len()
            );
            return Ok(());
        }

        let summary =
            fetch_anchor_account::<InventorySummaryAccount>(&program.rpc(), &inventory_summary)
                .await?;
        let sync_in_progress = summary
            .as_ref()
            .map(|summary| summary.sync_in_progress)
            .unwrap_or(false);
        let (entries, resume_info) = pending_inventory_entries_for_sync(
            entries,
            sync_in_progress,
            pool.inventory_sync_session_id,
        );
        if let Some(resume_info) = resume_info.as_ref() {
            println!(
                "Continuing inventory sync session {}: {}/{} active entries already processed; {} remaining",
                resume_info.session_id,
                resume_info.already_synced,
                resume_info.total,
                entries.len()
            );
        }

        if entries.is_empty() {
            if resume_info.is_some() {
                println!(
                    "No remaining active v3 LST inventory entries found for this sync session"
                );
            } else {
                println!("No active v3 LST inventory entries found for sync");
            }
            vec![vec![]]
        } else {
            println!("Syncing {} active v3 LST inventory entries:", entries.len());
            for entry in &entries {
                println!(
                    "  mint={} stake_pool={} pool_lst_token_account={} lst_info={} last_synced_session_id={}",
                    entry.mint,
                    entry.stake_pool,
                    entry.pool_lst_token_account,
                    entry.lst_info,
                    entry.last_synced_session_id
                );
            }
            entries
                .chunks(chunk_size)
                .map(|chunk| chunk.to_vec())
                .collect()
        }
    };

    if simulate && chunks.len() > 1 {
        return Err(anyhow!(
            "sync-inventory simulation only supports one chunk; use a larger --chunk-size"
        ));
    }

    for chunk in chunks.drain(..) {
        let remaining_accounts = chunk
            .into_iter()
            .flat_map(|entry| {
                [
                    AccountMeta::new_readonly(entry.pool_lst_token_account, false),
                    AccountMeta::new_readonly(entry.stake_pool, false),
                    AccountMeta::new(entry.lst_info, false),
                ]
            })
            .collect_vec();

        let instructions = program
            .request()
            .accounts(lu_client::accounts::SyncInventory {
                pool: *pool_id,
                maintenance_authority: wallet.pubkey(),
                inventory_summary,
                inventory_sync_state,
                system_program: solana_sdk::system_program::id(),
            })
            .accounts(remaining_accounts)
            .args(lu_client::args::SyncInventory {})
            .instructions()?;

        send_instructions(
            program,
            wallet,
            instructions,
            &[],
            simulate,
            Some(vec![inventory_summary, inventory_sync_state]),
        )
        .await?;
    }

    Ok(())
}

async fn inventory_status(program: &ProgramClient, pool_id: &Pubkey) -> Result<()> {
    let inventory_summary = inventory_summary_address(pool_id);
    let pool = fetch_pool(program, *pool_id).await?;
    let lst_info_records = list_lst_info_records(&program.rpc(), pool_id).await?;
    let lst_info_count = lst_info_records.len();
    let entries = active_inventory_entries_from_records(pool_id, lst_info_records);

    println!("InventorySummary {inventory_summary}");
    if let Some(summary) =
        fetch_anchor_account::<InventorySummaryAccount>(&program.rpc(), &inventory_summary).await?
    {
        println!("  total_value_snapshot={}", summary.total_value_snapshot);
        println!("  snapshot_epoch={}", summary.snapshot_epoch);
        println!("  snapshot_progress={}", summary.snapshot_progress);
        println!("  snapshot_slot={}", summary.snapshot_slot);
        println!("  sync_in_progress={}", summary.sync_in_progress);
    } else {
        println!("  missing");
    }
    println!("LstInfo accounts={lst_info_count}");
    println!("Active v3 inventory entries={}", entries.len());
    println!(
        "Pool active_lst_mints_count={}",
        pool.active_lst_mints_count
    );

    if let Some(reason) =
        inventory_sync_needed(&program.rpc(), pool_id, &pool, &inventory_summary, &entries).await?
    {
        println!("Status: needs update");
        println!("Reason: {reason}");
    } else {
        println!("Status: current");
    }

    Ok(())
}

async fn print_vlp_price(program: &ProgramClient, pool_id: &Pubkey) -> Result<()> {
    let rpc = program.rpc();
    let pool = fetch_pool(program, *pool_id).await?;
    if pool.flash_loan_borrowed_amount != 0 {
        return Err(anyhow!(
            "LP operations are blocked while a flash loan is active"
        ));
    }

    let epoch_info = rpc.get_epoch_info().await?;
    let epoch_progress =
        epoch_progress_from_slot_index(epoch_info.slot_index, epoch_info.slots_in_epoch)?;
    let inventory_summary_address = inventory_summary_address(pool_id);
    let inventory_summary =
        fetch_anchor_account::<InventorySummaryAccount>(&rpc, &inventory_summary_address).await?;
    let inventory_value = accrue_inventory_summary_value(
        inventory_summary.as_ref(),
        pool_id,
        &pool,
        epoch_info.epoch,
        epoch_progress,
    )?;

    let total_sol_in_vault_plus_pending = pool
        .sol_vault_lamports
        .checked_add(pool.total_deactivating_stake)
        .ok_or_else(|| anyhow!("pool value overflow"))?;
    let total_pool_value = total_sol_in_vault_plus_pending
        .checked_add(inventory_value)
        .ok_or_else(|| anyhow!("pool value overflow"))?;
    let unvested_rewards =
        calculate_unvested_stake_rewards(&pool, epoch_info.epoch, epoch_progress)?;
    let priced_pool_value = total_pool_value
        .checked_sub(unvested_rewards)
        .ok_or_else(|| anyhow!("pool value underflow after unvested rewards"))?;
    let one_vlp_withdraw_gross = (pool.total_lp_tokens != 0)
        .then(|| {
            calculate_lamports_to_withdraw_for_lp(
                pool.total_lp_tokens,
                LAMPORTS_PER_SOL,
                total_pool_value,
                0,
                unvested_rewards,
            )
        })
        .transpose()?;
    let one_sol_deposit_vlp_tokens = calculate_tokens_to_mint_for_deposit(
        pool.total_lp_tokens,
        LAMPORTS_PER_SOL,
        total_pool_value,
        unvested_rewards,
    )?;
    let one_sol_deposit_exceeds_cap = total_pool_value
        .checked_add(LAMPORTS_PER_SOL)
        .ok_or_else(|| anyhow!("pool value overflow"))?
        > pool.sol_vault_lamports_cap;

    println!("lp_mint={}", pool.lp_mint);
    println!("total_lp_tokens={}", pool.total_lp_tokens);
    println!("sol_vault_lamports={}", pool.sol_vault_lamports);
    println!(
        "total_deactivating_stake_lamports={}",
        pool.total_deactivating_stake
    );
    println!("inventory_summary={inventory_summary_address}");
    println!("inventory_value_lamports={inventory_value}");
    println!("total_pool_value_lamports={total_pool_value}");
    println!("unvested_rewards_lamports={unvested_rewards}");
    println!("priced_pool_value_lamports={priced_pool_value}");

    println!("one_sol_deposit_vlp_tokens={one_sol_deposit_vlp_tokens}");
    if one_sol_deposit_exceeds_cap {
        println!("one_sol_deposit_note=deposit would exceed pool.sol_vault_lamports_cap");
    } else if one_sol_deposit_vlp_tokens == 0 {
        println!("one_sol_deposit_note=deposit would fail with LpTokensToMintIsZero");
    }

    if let Some(gross_price) = one_vlp_withdraw_gross {
        println!("gross_price_lamports_per_vlp={gross_price}");
        println!(
            "gross_price_sol_per_vlp={}",
            format_lamports_as_sol(u128::from(gross_price))
        );

        let withdraw_sol_price = calculate_lamports_to_withdraw_for_lp(
            pool.total_lp_tokens,
            LAMPORTS_PER_SOL,
            total_pool_value,
            pool.withdraw_sol_fee,
            unvested_rewards,
        )?;
        let withdraw_stake_account_price = calculate_lamports_to_withdraw_for_lp(
            pool.total_lp_tokens,
            LAMPORTS_PER_SOL,
            total_pool_value,
            pool.withdraw_stake_account_fee,
            unvested_rewards,
        )?;
        println!("withdraw_sol_fee={}", pool.withdraw_sol_fee);
        println!("withdraw_sol_price_lamports_per_vlp={withdraw_sol_price}");
        println!(
            "withdraw_sol_price_sol_per_vlp={}",
            format_lamports_as_sol(u128::from(withdraw_sol_price))
        );
        if pool.sol_vault_lamports < withdraw_sol_price {
            println!("withdraw_sol_note=pool SOL vault cannot cover this withdrawal amount");
        }
        println!(
            "withdraw_stake_account_fee={}",
            pool.withdraw_stake_account_fee
        );
        println!("withdraw_stake_account_price_lamports_per_vlp={withdraw_stake_account_price}");
        println!(
            "withdraw_stake_account_price_sol_per_vlp={}",
            format_lamports_as_sol(u128::from(withdraw_stake_account_price))
        );
    } else {
        println!("price_note=no LP supply; there is no current withdrawable share price");
    }

    Ok(())
}

fn accrue_inventory_summary_value(
    summary: Option<&InventorySummaryAccount>,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    current_epoch: u64,
    epoch_progress: u64,
) -> Result<u64> {
    let Some(summary) = summary else {
        return Ok(0);
    };
    if summary.sync_in_progress {
        return Err(anyhow!(
            "inventory sync is in progress for {}; finish or abort sync-inventory before reading VLP price",
            inventory_summary_address(pool_id)
        ));
    }
    if summary.pool == Pubkey::default() {
        return Ok(0);
    }
    if summary.pool != *pool_id {
        return Err(anyhow!(
            "InventorySummary {} belongs to pool {}, expected {pool_id}",
            inventory_summary_address(pool_id),
            summary.pool
        ));
    }
    if summary.snapshot_epoch == 0 || summary.total_value_snapshot == 0 {
        return Ok(summary.total_value_snapshot);
    }
    if summary.snapshot_epoch != current_epoch {
        return Err(anyhow!(
            "inventory summary is from epoch {}, current epoch {}; run sync-inventory",
            summary.snapshot_epoch,
            current_epoch
        ));
    }
    if summary.snapshot_progress > epoch_progress {
        return Err(anyhow!(
            "inventory summary snapshot progress is ahead of current epoch progress"
        ));
    }

    let multiplier_now =
        calculate_inflation_multiplier(epoch_progress, pool.expected_inflation_per_epoch)?;
    let multiplier_snapshot = calculate_inflation_multiplier(
        summary.snapshot_progress,
        pool.expected_inflation_per_epoch,
    )?;

    Ok(u64::try_from(
        u128::from(summary.total_value_snapshot)
            .checked_mul(u128::from(multiplier_now))
            .ok_or_else(|| anyhow!("inventory accrual overflow"))?
            .checked_div(u128::from(multiplier_snapshot))
            .ok_or_else(|| anyhow!("inventory accrual underflow"))?,
    )
    .map_err(|_| anyhow!("inventory accrual overflow"))?)
}

fn calculate_unvested_stake_rewards(
    pool: &PoolAccount,
    current_epoch: u64,
    epoch_progress: u64,
) -> Result<u64> {
    if u64::from(pool.last_stake_rewards_withdrawn_epoch) != current_epoch {
        return Ok(0);
    }
    let vested = (pool.total_stake_rewards_withdrawn as u128)
        .checked_mul(epoch_progress as u128)
        .ok_or_else(|| anyhow!("reward vesting overflow"))?
        .checked_div(u64::MAX as u128)
        .ok_or_else(|| anyhow!("reward vesting underflow"))? as u64;
    pool.total_stake_rewards_withdrawn
        .checked_sub(vested)
        .ok_or_else(|| anyhow!("reward vesting underflow"))
}

fn calculate_tokens_to_mint_for_deposit(
    lp_mint_supply: u64,
    lamports_to_deposit: u64,
    lamports_total_in_pool: u64,
    unvested_rewards: u64,
) -> Result<u64> {
    if lamports_to_deposit == 0 {
        return Err(anyhow!("deposit amount must be greater than zero"));
    }

    if lp_mint_supply == 0 || lamports_total_in_pool == 0 {
        return lamports_to_deposit
            .checked_add(lamports_total_in_pool)
            .ok_or_else(|| anyhow!("deposit mint overflow"))?
            .checked_sub(lp_mint_supply)
            .ok_or_else(|| anyhow!("deposit mint underflow"));
    }

    let priced_pool_value = lamports_total_in_pool
        .checked_sub(unvested_rewards)
        .ok_or_else(|| anyhow!("pool value underflow after unvested rewards"))?;

    Ok(u64::try_from(
        u128::from(lp_mint_supply)
            .checked_mul(u128::from(lamports_to_deposit))
            .ok_or_else(|| anyhow!("deposit mint overflow"))?
            .checked_div(u128::from(priced_pool_value))
            .ok_or_else(|| anyhow!("deposit mint underflow"))?,
    )
    .map_err(|_| anyhow!("deposit mint overflow"))?)
}

fn calculate_lamports_to_withdraw_for_lp(
    lp_mint_supply: u64,
    lp_to_burn: u64,
    lamports_total_in_pool: u64,
    fee: u16,
    unvested_rewards: u64,
) -> Result<u64> {
    if lamports_total_in_pool == 0 || lp_mint_supply == 0 {
        return Ok(0);
    }

    let priced_pool_value = lamports_total_in_pool
        .checked_sub(unvested_rewards)
        .ok_or_else(|| anyhow!("pool value underflow after unvested rewards"))?;
    let base_amount = u64::try_from(
        u128::from(lp_to_burn)
            .checked_mul(u128::from(priced_pool_value))
            .ok_or_else(|| anyhow!("withdraw amount overflow"))?
            .checked_div(u128::from(lp_mint_supply))
            .ok_or_else(|| anyhow!("withdraw amount underflow"))?,
    )
    .map_err(|_| anyhow!("withdraw amount overflow"))?;

    let fee_lamports = calculate_withdraw_fee(base_amount, fee)?;
    base_amount
        .checked_sub(fee_lamports)
        .ok_or_else(|| anyhow!("withdraw amount underflow"))
}

fn calculate_withdraw_fee(amount: u64, fee: u16) -> Result<u64> {
    if fee > MAX_WITHDRAW_FEE {
        return Err(anyhow!("invalid withdraw fee"));
    }

    Ok(u64::try_from(
        u128::from(amount)
            .checked_mul(u128::from(fee))
            .ok_or_else(|| anyhow!("withdraw fee overflow"))?
            .checked_div(u128::from(FEE_PCT_DIVISOR))
            .ok_or_else(|| anyhow!("withdraw fee underflow"))?,
    )
    .map_err(|_| anyhow!("withdraw fee overflow"))?)
}

fn format_lamports_as_sol(lamports: u128) -> String {
    format_scaled_decimal(lamports, TOKEN_DECIMAL_FACTOR, 9)
}

fn format_scaled_decimal(value: u128, scale: u128, decimals: usize) -> String {
    let whole = value / scale;
    let fraction = value % scale;
    format!("{whole}.{fraction:0decimals$}")
}

#[derive(Clone)]
struct InventoryEntry {
    mint: Pubkey,
    pool_lst_token_account: Pubkey,
    stake_pool: Pubkey,
    lst_info: Pubkey,
    last_synced_session_id: u32,
    rate_history_epochs: [u64; 5],
    rate_history_rates: [u64; 5],
    rate_history_len: u8,
}

enum InventoryValueCheck {
    CurrentValue(u64),
    NeedsSync(String),
}

#[derive(Debug, PartialEq, Eq)]
struct InventorySyncResumeInfo {
    session_id: u32,
    already_synced: usize,
    total: usize,
}

fn pending_inventory_entries_for_sync(
    entries: Vec<InventoryEntry>,
    sync_in_progress: bool,
    session_id: u32,
) -> (Vec<InventoryEntry>, Option<InventorySyncResumeInfo>) {
    if !sync_in_progress {
        return (entries, None);
    }

    let total = entries.len();
    let (already_synced_entries, pending_entries): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| entry.last_synced_session_id == session_id);
    let resume_info = InventorySyncResumeInfo {
        session_id,
        already_synced: already_synced_entries.len(),
        total,
    };

    (pending_entries, Some(resume_info))
}

fn active_inventory_entries_from_records(
    pool_id: &Pubkey,
    entries: Vec<LstInfoRecord>,
) -> Vec<InventoryEntry> {
    let mut active = Vec::new();

    for record in entries {
        let Some(lst_info) = record.v3 else {
            continue;
        };
        if !lst_info.is_active {
            continue;
        }
        let pool_lst_token_account = token_account_address(pool_id, &lst_info.mint);
        active.push(InventoryEntry {
            mint: lst_info.mint,
            pool_lst_token_account,
            stake_pool: lst_info.stake_pool,
            lst_info: record.address,
            last_synced_session_id: lst_info.last_synced_session_id,
            rate_history_epochs: lst_info.rate_history_epochs,
            rate_history_rates: lst_info.rate_history_rates,
            rate_history_len: lst_info.rate_history_len,
        });
    }

    active
}

async fn inventory_sync_needed(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    inventory_summary: &Pubkey,
    entries: &[InventoryEntry],
) -> Result<Option<String>> {
    let Some(summary) =
        fetch_anchor_account::<InventorySummaryAccount>(rpc, inventory_summary).await?
    else {
        return Ok(Some(format!(
            "missing InventorySummary {inventory_summary}"
        )));
    };

    if summary.pool != *pool_id {
        return Ok(Some(format!(
            "InventorySummary {inventory_summary} belongs to pool {}, expected {pool_id}",
            summary.pool
        )));
    }

    if summary.sync_in_progress {
        return Ok(Some(format!(
            "inventory sync is already in progress for {inventory_summary}"
        )));
    }

    let epoch_info = rpc.get_epoch_info().await?;
    if summary.snapshot_epoch != epoch_info.epoch {
        return Ok(Some(format!(
            "inventory summary is from epoch {}, current epoch {}",
            summary.snapshot_epoch, epoch_info.epoch
        )));
    }

    if entries.len() != pool.active_lst_mints_count as usize {
        return Ok(Some(format!(
            "pool tracks {} active LST mints but found {} active v3 LST info accounts",
            pool.active_lst_mints_count,
            entries.len()
        )));
    }

    let current_value = match inventory_value_at_snapshot_progress(
        rpc,
        pool,
        entries,
        summary.snapshot_epoch,
        summary.snapshot_progress,
    )
    .await?
    {
        InventoryValueCheck::CurrentValue(value) => value,
        InventoryValueCheck::NeedsSync(reason) => return Ok(Some(reason)),
    };
    if current_value != summary.total_value_snapshot {
        return Ok(Some(format!(
            "inventory summary value is {}, current active inventory value is {}",
            summary.total_value_snapshot, current_value
        )));
    }

    Ok(None)
}

async fn inventory_value_at_snapshot_progress(
    rpc: &RpcClient,
    pool: &PoolAccount,
    entries: &[InventoryEntry],
    snapshot_epoch: u64,
    snapshot_progress: u64,
) -> Result<InventoryValueCheck> {
    let mut total_value = 0_u64;

    for chunk in entries.chunks(INVENTORY_SYNC_ENTRIES_PER_RPC_BATCH) {
        let account_keys = chunk
            .iter()
            .flat_map(|entry| [entry.pool_lst_token_account, entry.stake_pool])
            .collect_vec();
        let accounts = rpc.get_multiple_accounts(&account_keys).await?;
        if accounts.len() != account_keys.len() {
            return Err(anyhow!(
                "RPC returned {} accounts, expected {}",
                accounts.len(),
                account_keys.len()
            ));
        }

        for (entry, accounts) in chunk
            .iter()
            .zip(accounts.chunks_exact(INVENTORY_SYNC_ACCOUNTS_PER_ENTRY))
        {
            let lst_amount = accounts[0]
                .as_ref()
                .and_then(token_amount_from_account)
                .unwrap_or(0);
            if lst_amount == 0 {
                return Ok(InventoryValueCheck::NeedsSync(format!(
                    "active LST {} has zero or missing pool token account {}",
                    entry.mint, entry.pool_lst_token_account
                )));
            }

            let stake_pool_account = accounts[1]
                .as_ref()
                .ok_or_else(|| anyhow!("missing stake pool account {}", entry.stake_pool))?;
            let mut data = stake_pool_account.data.as_slice();
            let stake_pool_state = StakePool::deserialize(&mut data).map_err(|err| {
                anyhow!(
                    "failed to deserialize stake pool account {}: {err}",
                    entry.stake_pool
                )
            })?;

            let current_rate = calculate_stake_pool_rate(&stake_pool_state)?;
            if !lst_info_has_recorded_rate(entry, snapshot_epoch, current_rate) {
                return Ok(InventoryValueCheck::NeedsSync(format!(
                    "LST {} has not recorded current epoch {} rate {}",
                    entry.mint, snapshot_epoch, current_rate
                )));
            }

            let value = calculate_lst_sol_value(
                lst_amount,
                stake_pool_state.total_lamports,
                stake_pool_state.pool_token_supply,
                snapshot_progress,
                pool.expected_inflation_per_epoch,
            )?;
            total_value = total_value
                .checked_add(value)
                .ok_or_else(|| anyhow!("inventory value overflow"))?;
        }
    }

    Ok(InventoryValueCheck::CurrentValue(total_value))
}

fn calculate_stake_pool_rate(stake_pool: &StakePool) -> Result<u64> {
    if stake_pool.pool_token_supply == 0 {
        return Err(anyhow!("stake pool token supply is zero"));
    }

    Ok((stake_pool.total_lamports as u128)
        .checked_mul(RATE_SCALE)
        .ok_or_else(|| anyhow!("rate overflow"))?
        .checked_div(stake_pool.pool_token_supply as u128)
        .ok_or_else(|| anyhow!("rate underflow"))? as u64)
}

fn lst_info_has_recorded_rate(entry: &InventoryEntry, epoch: u64, rate: u64) -> bool {
    (0..(entry.rate_history_len as usize).min(entry.rate_history_epochs.len()))
        .any(|i| entry.rate_history_epochs[i] == epoch && entry.rate_history_rates[i] == rate)
}

async fn update(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    stake_accounts: Option<&[Pubkey]>,
    chunk_size: usize,
    simulate: bool,
) -> Result<()> {
    if chunk_size == 0 {
        return Err(anyhow!("update chunk-size must be greater than zero"));
    }

    let stake_account_infos =
        stake_account_infos_for_update(&program.rpc(), pool_id, stake_accounts).await?;
    if stake_account_infos.is_empty() {
        return Err(anyhow!("no tracked stake accounts found for update"));
    }

    let total_chunks = stake_account_infos.len().div_ceil(chunk_size);
    for (chunk_index, chunk) in stake_account_infos.chunks(chunk_size).enumerate() {
        if total_chunks > 1 {
            println!(
                "Processing update chunk {}/{} ({} stake accounts)",
                chunk_index + 1,
                total_chunks,
                chunk.len()
            );
        }
        update_chunk(program, pool_id, wallet, chunk, simulate).await?;
    }

    Ok(())
}

async fn update_chunk(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    stake_account_infos: &[(Pubkey, Pubkey)],
    simulate: bool,
) -> Result<()> {
    let mut remaining_accounts = stake_account_infos
        .iter()
        .map(|(info, _stake_account)| AccountMeta::new(*info, false))
        .collect_vec();
    remaining_accounts.extend(
        stake_account_infos
            .iter()
            .map(|(_info, stake_account)| AccountMeta::new(*stake_account, false)),
    );

    let instructions = program
        .request()
        .accounts(lu_client::accounts::Update {
            pool: *pool_id,
            sol_vault: sol_vault_address(pool_id),
            stake_program: solana_sdk::stake::program::id(),
            token_program: spl_token::id(),
            clock: solana_sdk::sysvar::clock::id(),
            stake_history: solana_sdk::sysvar::stake_history::id(),
            system_program: solana_sdk::system_program::id(),
        })
        .accounts(remaining_accounts)
        .args(lu_client::args::Update {})
        .instructions()?;

    let mut simulation_accounts = vec![*pool_id, sol_vault_address(pool_id)];
    simulation_accounts.extend(
        stake_account_infos
            .iter()
            .map(|(_, stake_account)| *stake_account),
    );
    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(simulation_accounts),
    )
    .await
}

async fn stake_account_infos_for_update(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    stake_accounts: Option<&[Pubkey]>,
) -> Result<Vec<(Pubkey, Pubkey)>> {
    if let Some(stake_accounts) = stake_accounts {
        let mut entries = stake_accounts
            .iter()
            .map(|stake_account| (stake_account_info_address(stake_account), *stake_account))
            .collect_vec();
        entries.sort_by_key(|(_, stake_account)| *stake_account);
        return Ok(entries);
    }

    let accounts = rpc
        .get_program_accounts_with_config(
            &ID_CONST,
            RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..RpcAccountInfoConfig::default()
                },
                filters: Some(vec![
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        0,
                        StakeAccountInfoAccount::DISCRIMINATOR,
                    )),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(40, &pool_id.to_bytes())),
                ]),
                ..RpcProgramAccountsConfig::default()
            },
        )
        .await?;

    let mut entries = Vec::new();
    for (address, account) in accounts {
        let mut data = account.data.as_slice();
        let stake_info = StakeAccountInfoAccount::try_deserialize(&mut data)?;
        entries.push((address, stake_info.stake_account));
    }
    entries.sort_by_key(|(_, stake_account)| *stake_account);
    Ok(entries)
}

async fn unstake_pool_lsts_balanced(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    pool: &PoolAccount,
    cap_percent: u32,
    overrides: &[PoolLstTargetOverride],
    stake_account_seed: Option<u64>,
    simulate: bool,
) -> Result<()> {
    let epoch_progress = get_epoch_progress(&program.rpc()).await?;
    let positions = list_pool_lst_position_snapshots(
        &program.rpc(),
        pool_id,
        pool.expected_inflation_per_epoch,
        epoch_progress,
    )
    .await?;
    if positions.is_empty() {
        println!("No pool-owned LST balances found");
        return Ok(());
    }
    let minimum_unstake_lamports =
        spl_stake_pool::minimum_delegation(program.rpc().get_stake_minimum_delegation().await?);

    let plan = build_balanced_pool_lst_plan(
        pool.sol_vault_lamports,
        pool.total_deactivating_stake,
        pool.expected_inflation_per_epoch,
        epoch_progress,
        cap_percent,
        minimum_unstake_lamports,
        positions,
        overrides,
    )?;
    if let Some(message) = balanced_pool_lst_skip_message(&plan) {
        println!("{message}");
        return Ok(());
    }
    print_balanced_pool_lst_plan(&plan);

    let requests = plan
        .positions
        .iter()
        .filter(|position| position.unstake_amount > 0)
        .map(|position| PoolLstUnstakeRequest {
            mint: position.mint,
            amount: position.unstake_amount,
            stake_pool_program_id: Some(position.stake_pool_program_id),
        })
        .collect_vec();

    if requests.is_empty() {
        println!("No pool-owned LST balances need unstaking for this plan");
        return Ok(());
    }

    execute_pool_lst_unstake_requests(
        program,
        pool_id,
        wallet,
        requests,
        stake_account_seed,
        simulate,
    )
    .await
}

async fn list_pool_lst_position_snapshots(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    expected_inflation_per_epoch: u32,
    epoch_progress: u64,
) -> Result<Vec<PoolLstPositionSnapshot>> {
    let balances = list_pool_lst_balances(rpc, pool_id).await?;
    let mut positions = Vec::with_capacity(balances.len());

    for (mint, amount) in balances {
        let (stake_pool_program_id, (_stake_pool_address, stake_pool_state)) =
            get_stake_pool_for_mint_from_supported_programs(rpc, &mint).await?;
        let sol_value = calculate_lst_sol_value(
            amount,
            stake_pool_state.total_lamports,
            stake_pool_state.pool_token_supply,
            epoch_progress,
            expected_inflation_per_epoch,
        )?;
        positions.push(PoolLstPositionSnapshot {
            mint,
            amount,
            sol_value,
            stake_pool_program_id,
            stake_pool_total_lamports: stake_pool_state.total_lamports,
            stake_pool_token_supply: stake_pool_state.pool_token_supply,
            stake_withdrawal_fee: stake_pool_state.stake_withdrawal_fee,
        });
    }

    positions.sort_by_key(|position| position.mint);
    Ok(positions)
}

fn build_balanced_pool_lst_plan(
    sol_vault_lamports: u64,
    total_deactivating_stake_lamports: u64,
    expected_inflation_per_epoch: u32,
    epoch_progress: u64,
    cap_percent: u32,
    minimum_unstake_lamports: u64,
    positions: Vec<PoolLstPositionSnapshot>,
    overrides: &[PoolLstTargetOverride],
) -> Result<BalancedPoolLstPlan> {
    let positions_by_mint = positions
        .iter()
        .map(|position| (position.mint, position))
        .collect::<HashMap<_, _>>();
    let mut override_targets = HashMap::new();
    for override_target in overrides {
        let position = positions_by_mint
            .get(&override_target.mint)
            .ok_or_else(|| {
                anyhow!(
                    "--lst-target supplied mint {} but the pool owns no tokens for that mint",
                    override_target.mint
                )
            })?;
        let inserted = override_targets
            .insert(override_target.mint, override_target.percent)
            .is_none();
        if !inserted {
            return Err(anyhow!(
                "duplicate --lst-target for mint {}",
                override_target.mint
            ));
        }
        if position.sol_value == 0 && override_target.percent > 0 {
            return Err(anyhow!(
                "LST {} has zero SOL value, cannot apply a positive target",
                override_target.mint
            ));
        }
    }

    let current_lst_value_lamports = positions.iter().try_fold(0_u128, |total, position| {
        total
            .checked_add(u128::from(position.sol_value))
            .ok_or_else(|| anyhow!("LST value overflow"))
    })?;
    let tvl_lamports = u128::from(sol_vault_lamports)
        .checked_add(u128::from(total_deactivating_stake_lamports))
        .ok_or_else(|| anyhow!("pool TVL overflow"))?
        .checked_add(current_lst_value_lamports)
        .ok_or_else(|| anyhow!("pool TVL overflow"))?;
    let target_lst_value_lamports = percent_of_u128(tvl_lamports, cap_percent)?;
    let trigger_percent = cap_percent
        .checked_add(BALANCED_LST_CAP_TRIGGER_BUFFER_PERCENT)
        .map(|percent| percent.min(PERCENT_SCALE as u32))
        .ok_or_else(|| anyhow!("balanced LST cap trigger percentage overflow"))?;
    let trigger_lst_value_lamports = percent_of_u128(tvl_lamports, trigger_percent)?;
    let global_reduction_needed = current_lst_value_lamports > trigger_lst_value_lamports;

    let mut fixed_target_values = HashMap::<Pubkey, u128>::new();
    let mut fixed_target_sum = 0_u128;
    for position in &positions {
        let Some(percent) = override_targets.get(&position.mint).copied() else {
            continue;
        };
        let target_value =
            percent_of_u128(tvl_lamports, percent)?.min(u128::from(position.sol_value));
        fixed_target_sum = fixed_target_sum
            .checked_add(target_value)
            .ok_or_else(|| anyhow!("override target value overflow"))?;
        fixed_target_values.insert(position.mint, target_value);
    }

    if global_reduction_needed && fixed_target_sum > target_lst_value_lamports {
        return Err(anyhow!(
            "--lst-target overrides require {} lamports in overridden LSTs, above global LST cap target {} lamports",
            fixed_target_sum,
            target_lst_value_lamports
        ));
    }

    let non_override_current_value = positions
        .iter()
        .filter(|position| !fixed_target_values.contains_key(&position.mint))
        .try_fold(0_u128, |total, position| {
            total
                .checked_add(u128::from(position.sol_value))
                .ok_or_else(|| anyhow!("non-overridden LST value overflow"))
        })?;
    let non_override_target_value = if global_reduction_needed {
        target_lst_value_lamports
            .checked_sub(fixed_target_sum)
            .ok_or_else(|| anyhow!("override target value exceeds global cap"))?
            .min(non_override_current_value)
    } else {
        non_override_current_value
    };

    let mut plan_positions = Vec::with_capacity(positions.len());
    let mut new_lst_value_lamports = 0_u128;
    for position in positions {
        let mut target_value = if global_reduction_needed {
            if let Some(target_value) = fixed_target_values.get(&position.mint) {
                *target_value
            } else if non_override_current_value > 0 {
                u128::from(position.sol_value)
                    .checked_mul(non_override_target_value)
                    .ok_or_else(|| anyhow!("target value overflow"))?
                    .checked_div(non_override_current_value)
                    .ok_or_else(|| anyhow!("target value underflow"))?
            } else {
                u128::from(position.sol_value)
            }
        } else {
            u128::from(position.sol_value)
        }
        .min(u128::from(position.sol_value));
        let mut note = None;
        if global_reduction_needed
            && target_value > 0
            && target_value < u128::from(LAMPORTS_PER_SOL)
        {
            note = Some("target below 1 SOL; planning full LST unstake".to_string());
            target_value = 0;
        }

        let mut target_amount = calculate_position_target_lst_amount(
            &position,
            target_value,
            expected_inflation_per_epoch,
            epoch_progress,
        )?;
        let mut target_sol_value = if target_amount == position.amount {
            position.sol_value
        } else {
            calculate_lst_sol_value(
                target_amount,
                position.stake_pool_total_lamports,
                position.stake_pool_token_supply,
                epoch_progress,
                expected_inflation_per_epoch,
            )?
        };
        let mut unstake_amount = position
            .amount
            .checked_sub(target_amount)
            .ok_or_else(|| anyhow!("target amount exceeds current amount"))?;
        let mut unstake_sol_lamports =
            calculate_position_unstake_lamports(&position, unstake_amount)?;
        if unstake_amount > 0 && unstake_sol_lamports < minimum_unstake_lamports {
            note = Some(format!(
                "skipped: estimated unstake split {} lamports is below minimum {}",
                unstake_sol_lamports, minimum_unstake_lamports
            ));
            target_amount = position.amount;
            target_sol_value = position.sol_value;
            unstake_amount = 0;
            unstake_sol_lamports = 0;
        }
        new_lst_value_lamports = new_lst_value_lamports
            .checked_add(u128::from(target_sol_value))
            .ok_or_else(|| anyhow!("new LST value overflow"))?;

        plan_positions.push(BalancedPoolLstPlanPosition {
            mint: position.mint,
            current_amount: position.amount,
            current_sol_value: position.sol_value,
            current_sol_pct: ratio_percent_units(u128::from(position.sol_value), tvl_lamports),
            target_amount,
            target_sol_value,
            target_sol_pct: ratio_percent_units(u128::from(target_sol_value), tvl_lamports),
            unstake_amount,
            unstake_sol_lamports,
            override_percent: override_targets.get(&position.mint).copied(),
            stake_pool_program_id: position.stake_pool_program_id,
            note,
        });
    }

    Ok(BalancedPoolLstPlan {
        cap_percent,
        trigger_percent,
        sol_vault_lamports,
        total_deactivating_stake_lamports,
        current_lst_value_lamports,
        target_lst_value_lamports,
        trigger_lst_value_lamports,
        new_lst_value_lamports,
        tvl_lamports,
        minimum_unstake_lamports,
        positions: plan_positions,
    })
}

fn calculate_position_unstake_lamports(
    position: &PoolLstPositionSnapshot,
    unstake_amount: u64,
) -> Result<u64> {
    if unstake_amount == 0 {
        return Ok(0);
    }
    if position.stake_pool_token_supply == 0 {
        return Err(anyhow!("stake pool token supply is zero"));
    }
    let fee = position
        .stake_withdrawal_fee
        .apply(unstake_amount)
        .ok_or_else(|| anyhow!("stake withdrawal fee overflow"))?;
    let fee = u64::try_from(fee).map_err(|_| anyhow!("stake withdrawal fee overflow"))?;
    let net_amount = unstake_amount
        .checked_sub(fee)
        .ok_or_else(|| anyhow!("stake withdrawal fee exceeds unstake amount"))?;
    let numerator = u128::from(net_amount)
        .checked_mul(u128::from(position.stake_pool_total_lamports))
        .ok_or_else(|| anyhow!("stake withdraw lamports overflow"))?;
    if numerator < u128::from(position.stake_pool_token_supply) {
        return Ok(0);
    }
    Ok(u64::try_from(
        numerator
            .checked_div(u128::from(position.stake_pool_token_supply))
            .ok_or_else(|| anyhow!("stake withdraw lamports underflow"))?,
    )
    .map_err(|_| anyhow!("stake withdraw lamports overflow"))?)
}

fn calculate_position_target_lst_amount(
    position: &PoolLstPositionSnapshot,
    target_sol_value: u128,
    expected_inflation_per_epoch: u32,
    epoch_progress: u64,
) -> Result<u64> {
    if target_sol_value >= u128::from(position.sol_value) {
        return Ok(position.amount);
    }
    if target_sol_value == 0 {
        return Ok(0);
    }
    let target_sol_value = u64::try_from(target_sol_value)
        .map_err(|_| anyhow!("target SOL value exceeds u64 lamports"))?;
    Ok(calculate_lst_amount_for_sol_value_parts(
        target_sol_value,
        position.stake_pool_total_lamports,
        position.stake_pool_token_supply,
        expected_inflation_per_epoch,
        epoch_progress,
    )?
    .min(position.amount))
}

fn print_balanced_pool_lst_plan(plan: &BalancedPoolLstPlan) {
    println!("Pool LST balanced unstake plan");
    println!(
        "  sol_vault_lamports={} sol_vault_sol={}",
        plan.sol_vault_lamports,
        format_lamports_as_sol(u128::from(plan.sol_vault_lamports))
    );
    println!(
        "  total_deactivating_stake_lamports={} total_deactivating_stake_sol={}",
        plan.total_deactivating_stake_lamports,
        format_lamports_as_sol(u128::from(plan.total_deactivating_stake_lamports))
    );
    println!(
        "  current_lst_value_lamports={} current_lst_value_sol={} current_lst_pct={}",
        plan.current_lst_value_lamports,
        format_lamports_as_sol(plan.current_lst_value_lamports),
        format_percent_units(ratio_percent_units(
            plan.current_lst_value_lamports,
            plan.tvl_lamports,
        ))
    );
    println!(
        "  tvl_lamports={} tvl_sol={}",
        plan.tvl_lamports,
        format_lamports_as_sol(plan.tvl_lamports)
    );
    println!(
        "  lst_cap_pct={} target_lst_value_lamports={} target_lst_value_sol={}",
        format_percent_units(plan.cap_percent),
        plan.target_lst_value_lamports,
        format_lamports_as_sol(plan.target_lst_value_lamports)
    );
    println!(
        "  lst_trigger_pct={} trigger_lst_value_lamports={} trigger_lst_value_sol={}",
        format_percent_units(plan.trigger_percent),
        plan.trigger_lst_value_lamports,
        format_lamports_as_sol(plan.trigger_lst_value_lamports)
    );
    println!(
        "  minimum_unstake_lamports={} minimum_unstake_sol={}",
        plan.minimum_unstake_lamports,
        format_lamports_as_sol(u128::from(plan.minimum_unstake_lamports))
    );
    println!(
        "  planned_new_lst_value_lamports={} planned_new_lst_value_sol={} planned_new_lst_pct={}",
        plan.new_lst_value_lamports,
        format_lamports_as_sol(plan.new_lst_value_lamports),
        format_percent_units(ratio_percent_units(
            plan.new_lst_value_lamports,
            plan.tvl_lamports,
        ))
    );
    println!("mint,current_amount,current_sol_lamports,current_sol_pct,new_amount,new_sol_lamports,new_sol_pct,unstake_amount,unstake_sol_lamports,override_pct,note");
    for position in &plan.positions {
        println!(
            "{},{},{},{},{},{},{},{},{},{},{}",
            position.mint,
            position.current_amount,
            position.current_sol_value,
            format_percent_units(position.current_sol_pct),
            position.target_amount,
            position.target_sol_value,
            format_percent_units(position.target_sol_pct),
            position.unstake_amount,
            position.unstake_sol_lamports,
            position
                .override_percent
                .map(format_percent_units)
                .unwrap_or_default(),
            csv_escape(position.note.as_deref().unwrap_or(""))
        );
    }
}

fn balanced_pool_lst_skip_message(plan: &BalancedPoolLstPlan) -> Option<String> {
    if plan.current_lst_value_lamports > plan.trigger_lst_value_lamports {
        return None;
    }

    let current_pct = ratio_percent_units(plan.current_lst_value_lamports, plan.tvl_lamports);
    let comparison = if plan.current_lst_value_lamports < plan.trigger_lst_value_lamports {
        "less than"
    } else {
        "equal to"
    };
    Some(format!(
        "Current LST value {} is {} trigger {}; skipping unstakes",
        format_percent_units(current_pct),
        comparison,
        format_percent_units(plan.trigger_percent)
    ))
}

fn percent_of_u128(value: u128, percent: u32) -> Result<u128> {
    value
        .checked_mul(u128::from(percent))
        .ok_or_else(|| anyhow!("percentage calculation overflow"))?
        .checked_div(PERCENT_SCALE)
        .ok_or_else(|| anyhow!("percentage calculation underflow"))
}

fn ratio_percent_units(value: u128, total: u128) -> u32 {
    if total == 0 {
        return 0;
    }
    value
        .saturating_mul(PERCENT_SCALE)
        .checked_div(total)
        .unwrap_or(PERCENT_SCALE)
        .min(PERCENT_SCALE) as u32
}

fn format_percent_units(percent: u32) -> String {
    let whole = u128::from(percent) / PERCENT_UNITS_PER_ONE_PERCENT;
    let fraction = u128::from(percent) % PERCENT_UNITS_PER_ONE_PERCENT;
    if fraction == 0 {
        return format!("{whole}%");
    }

    let mut formatted = format!("{whole}.{fraction:04}");
    while formatted.ends_with('0') {
        formatted.pop();
    }
    format!("{formatted}%")
}

async fn unstake_pool_lsts_for_selection(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    mint_selection: PoolLstMintSelection,
    amount_selection: PoolLstAmountSelection,
    stake_account_seed: Option<u64>,
    simulate: bool,
) -> Result<()> {
    let requests = resolve_pool_lst_unstake_requests(
        &program.rpc(),
        pool_id,
        mint_selection,
        amount_selection,
    )
    .await?
    .into_iter()
    .map(|(mint, amount)| PoolLstUnstakeRequest {
        mint,
        amount,
        stake_pool_program_id: None,
    })
    .collect_vec();
    if requests.is_empty() {
        println!("No pool-owned LST balances found");
        return Ok(());
    }

    execute_pool_lst_unstake_requests(
        program,
        pool_id,
        wallet,
        requests,
        stake_account_seed,
        simulate,
    )
    .await
}

async fn execute_pool_lst_unstake_requests(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    requests: Vec<PoolLstUnstakeRequest>,
    stake_account_seed: Option<u64>,
    simulate: bool,
) -> Result<()> {
    let mut next_stake_account_seed = stake_account_seed;
    for request in requests {
        let pool = fetch_pool(program, *pool_id).await?;
        let seed = next_stake_account_seed.unwrap_or(pool.total_deactivating_stake);
        let stake_pool_program_id =
            if let Some(stake_pool_program_id) = request.stake_pool_program_id {
                stake_pool_program_id
            } else {
                get_stake_pool_for_mint_from_supported_programs(&program.rpc(), &request.mint)
                    .await?
                    .0
            };

        println!(
            "Unstaking {} tokens for mint {}",
            request.amount, request.mint
        );
        let new_stake_account_count = unstake_pool_lsts(
            program,
            pool_id,
            wallet,
            &stake_pool_program_id,
            &request.mint,
            &pool,
            request.amount,
            seed,
            simulate,
        )
        .await?;

        if stake_account_seed.is_some() || simulate {
            next_stake_account_seed = Some(
                seed.checked_add(new_stake_account_count as u64)
                    .ok_or_else(|| anyhow!("stake-account-seed overflow"))?,
            );
        }
    }

    Ok(())
}

async fn resolve_pool_lst_unstake_requests(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    mint_selection: PoolLstMintSelection,
    amount_selection: PoolLstAmountSelection,
) -> Result<Vec<(Pubkey, u64)>> {
    let needs_balances = matches!(mint_selection, PoolLstMintSelection::All)
        || matches!(amount_selection, PoolLstAmountSelection::All);
    let balances = if needs_balances {
        list_pool_lst_balances(rpc, pool_id).await?
    } else {
        Vec::new()
    };

    match (mint_selection, amount_selection) {
        (PoolLstMintSelection::One(mint), PoolLstAmountSelection::Amount(amount)) => {
            Ok(vec![(mint, amount)])
        }
        (PoolLstMintSelection::One(mint), PoolLstAmountSelection::All) => {
            let amount = balances
                .iter()
                .find_map(|(balance_mint, amount)| (*balance_mint == mint).then_some(*amount))
                .ok_or_else(|| anyhow!("pool owns no LST tokens for mint {mint}"))?;
            Ok(vec![(mint, amount)])
        }
        (PoolLstMintSelection::All, PoolLstAmountSelection::All) => Ok(balances),
        (PoolLstMintSelection::All, PoolLstAmountSelection::Amount(amount)) => {
            let underfunded = balances
                .iter()
                .filter(|(_, balance)| *balance < amount)
                .map(|(mint, balance)| format!("{mint} has {balance}"))
                .collect_vec();
            if !underfunded.is_empty() {
                return Err(anyhow!(
                    "not all pool-owned LST balances can cover amount {amount}: {}",
                    underfunded.join(", ")
                ));
            }
            Ok(balances
                .into_iter()
                .map(|(mint, _balance)| (mint, amount))
                .collect())
        }
    }
}

async fn unstake_pool_lsts(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    spl_stake_pool_program_id: &Pubkey,
    mint: &Pubkey,
    pool: &PoolAccount,
    amount: u64,
    stake_account_seed: u64,
    simulate: bool,
) -> Result<usize> {
    let rpc = program.rpc();
    let (spl_stake_pool_address, spl_stake_pool_state) =
        get_stake_pool_for_lst_mint(&rpc, mint, spl_stake_pool_program_id).await?;
    let spl_stake_pool_validator_list = rpc
        .get_account(&spl_stake_pool_state.validator_list)
        .await
        .map(|account| {
            let mut data = account.data.as_slice();
            spl_stake_pool::state::ValidatorList::deserialize(&mut data)
        })??;

    let (lst_amounts, withdraw_stake_accounts, new_stake_accounts, new_stake_pda_accounts) =
        get_unstake_accounts_with_new_stake_account_as_pda(
            spl_stake_pool_program_id,
            &spl_stake_pool_address,
            &spl_stake_pool_state,
            &spl_stake_pool_validator_list,
            stake_account_seed,
            &wallet.pubkey(),
            amount,
        )?;

    let lst_amounts = pad_lst_amounts(lst_amounts)?;
    let new_stake_account_count = new_stake_accounts.len();

    let pool_lst_token_account = token_account_address(pool_id, &spl_stake_pool_state.pool_mint);
    let stake_pool_withdraw_authority = Pubkey::find_program_address(
        &[&spl_stake_pool_address.to_bytes(), b"withdraw"],
        spl_stake_pool_program_id,
    )
    .0;
    let lst_info = lst_info_address(pool_id, mint);
    let inventory_summary = inventory_summary_address(pool_id);

    let mut instructions = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_000_000),
        create_ata_idempotent_ix(&wallet.pubkey(), pool_id, mint),
    ];
    instructions.extend(
        program
            .request()
            .accounts(lu_client::accounts::UnstakePoolLsts {
                pool: *pool_id,
                maintenance_authority: wallet.pubkey(),
                pool_lst_token_account,
                payer: wallet.pubkey(),
                stake_pool: spl_stake_pool_address,
                stake_pool_validator_list: spl_stake_pool_state.validator_list,
                stake_pool_withdraw_authority,
                stake_pool_manager_fee_account: spl_stake_pool_state.manager_fee_account,
                stake_pool_mint: spl_stake_pool_state.pool_mint,
                stake_pool_program: *spl_stake_pool_program_id,
                lst_info,
                inventory_summary,
                token_program: spl_token::id(),
                stake_program: solana_sdk::stake::program::id(),
                system_program: solana_sdk::system_program::id(),
                clock: solana_sdk::sysvar::clock::id(),
            })
            .accounts(vec![
                withdraw_stake_accounts
                    .into_iter()
                    .map(|x| AccountMeta::new(x, false))
                    .collect_vec(),
                new_stake_accounts
                    .iter()
                    .map(|x| AccountMeta::new(x.pubkey(), false))
                    .collect_vec(),
                new_stake_pda_accounts
                    .into_iter()
                    .map(|x| AccountMeta::new(x, false))
                    .collect_vec(),
            ])
            .args(lu_client::args::UnstakePoolLsts {
                lst_amounts,
                stake_account_seed: Some(stake_account_seed),
            })
            .instructions()?,
    );

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![
            wallet.pubkey(),
            pool_lst_token_account,
            pool.sol_vault,
            new_stake_accounts[0].pubkey(),
        ]),
    )
    .await?;

    Ok(new_stake_account_count)
}

async fn update_pool(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    args: &clap::ArgMatches,
    simulate: bool,
) -> Result<()> {
    let manager_fee_account = parse_pubkey(
        args.get_one::<String>("manager-fee-account").unwrap(),
        "manager-fee-account",
    )?;
    let maintenance_authority = parse_pubkey(
        args.get_one::<String>("maintenance-authority").unwrap(),
        "maintenance-authority",
    )?;
    let instructions = program
        .request()
        .accounts(lu_client::accounts::UpdatePool {
            pool: *pool_id,
            authority: wallet.pubkey(),
            manager_fee_account,
            system_program: solana_sdk::system_program::id(),
            token_program: spl_token::id(),
            rent: solana_sdk::sysvar::rent::id(),
        })
        .args(lu_client::args::UpdatePool {
            fee_max: *args.get_one::<u32>("fee-max").unwrap(),
            fee_min: *args.get_one::<u32>("fee-min").unwrap(),
            min_sol_for_min_fee: *args.get_one::<u64>("min-sol-for-min-fee").unwrap(),
            manager_fee_pct: *args.get_one::<u8>("manager-fee-pct").unwrap(),
            vault_lamports_cap: *args.get_one::<u64>("vault-lamports-cap").unwrap(),
            withdraw_sol_fee: *args.get_one::<u16>("withdraw-sol-fee").unwrap(),
            withdraw_stake_account_fee: *args.get_one::<u16>("withdraw-stake-account-fee").unwrap(),
            flash_loans_enabled: *args.get_one::<bool>("flash-loans-enabled").unwrap(),
            flash_loan_fee: *args.get_one::<u32>("flash-loan-fee").unwrap(),
            sell_lst_flat_fee: *args.get_one::<u32>("sell-lst-flat-fee").unwrap(),
            buy_lst_flat_fee: *args.get_one::<u32>("buy-lst-flat-fee").unwrap(),
            buy_lst_dynamic_fee_max: *args.get_one::<u32>("buy-lst-dynamic-fee-max").unwrap(),
            expected_inflation_per_epoch: *args
                .get_one::<u32>("expected-inflation-per-epoch")
                .unwrap(),
            max_epoch_progress_pct: *args.get_one::<u8>("max-epoch-progress-pct").unwrap(),
            min_buy_lamports: *args.get_one::<u64>("min-buy-lamports").unwrap(),
            max_rate_drift_bps: *args.get_one::<u16>("max-rate-drift-bps").unwrap(),
            maintenance_authority,
        })
        .instructions()?;

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![*pool_id]),
    )
    .await
}

async fn halt_pool(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    halted: bool,
    simulate: bool,
) -> Result<()> {
    let instructions = program
        .request()
        .accounts(lu_client::accounts::HaltPool {
            pool: *pool_id,
            authority: wallet.pubkey(),
        })
        .args(lu_client::args::HaltPool { halted })
        .instructions()?;

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![*pool_id]),
    )
    .await
}

async fn create_or_update_token_metadata(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Wallet,
    token_mint: Pubkey,
    name: String,
    symbol: String,
    uri: String,
    simulate: bool,
) -> Result<()> {
    let metadata_info = token_metadata_address(&token_mint);
    let instructions = program
        .request()
        .accounts(lu_client::accounts::CreateOrUpdateTokenMetadata {
            pool: *pool_id,
            authority: wallet.pubkey(),
            payer: wallet.pubkey(),
            token_mint,
            metadata_program: MPL_TOKEN_METADATA_PROGRAM,
            metadata_info,
            system_program: solana_sdk::system_program::id(),
        })
        .args(lu_client::args::CreateOrUpdateTokenMetadata { name, symbol, uri })
        .instructions()?;

    send_instructions(
        program,
        wallet,
        instructions,
        &[],
        simulate,
        Some(vec![*pool_id, token_mint, metadata_info]),
    )
    .await
}

async fn unstake_lst(
    program: &ProgramClient,
    unstake_pool_id: &Pubkey,
    wallet_keypair: &Wallet,
    spl_stake_pool_program_id: &Pubkey,
    mint: &Pubkey,
    unstake_pool_info: &PoolAccount,
    amount: u64,
    minimum_lamports_out: Option<u64>,
    simulate: bool,
    new_stake_account_as_pda: bool,
) -> Result<()> {
    let rpc = program.rpc();
    let (spl_stake_pool_address, spl_stake_pool_state) =
        get_stake_pool_for_lst_mint(&rpc, mint, spl_stake_pool_program_id).await?;

    assert_eq!(spl_stake_pool_state.pool_mint, *mint);

    let spl_stake_pool_validator_list = rpc
        .get_account(&spl_stake_pool_state.validator_list)
        .await
        .map(|account| {
            let mut data = account.data.as_slice();
            spl_stake_pool::state::ValidatorList::deserialize(&mut data)
        })??;

    let stake_account_seed = unstake_pool_info.total_deactivating_stake;

    let (lst_amounts, withdraw_stake_accounts, new_stake_accounts, new_stake_pda_accounts) =
        if new_stake_account_as_pda {
            get_unstake_accounts_with_new_stake_account_as_pda(
                spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                stake_account_seed,
                &wallet_keypair.pubkey(),
                amount,
            )?
        } else {
            get_unstake_accounts(
                spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                amount,
            )?
        };

    let lst_amounts = pad_lst_amounts(lst_amounts)?;

    let wallet_lst_token_ata =
        token_account_address(&wallet_keypair.pubkey(), &spl_stake_pool_state.pool_mint);
    let stake_pool_withdraw_authority = Pubkey::find_program_address(
        &[&spl_stake_pool_address.to_bytes(), b"withdraw"],
        spl_stake_pool_program_id,
    )
    .0;

    let mut instructions = vec![create_ata_idempotent_ix(
        &wallet_keypair.pubkey(),
        &wallet_keypair.pubkey(),
        mint,
    )];

    let builder = program
        .request()
        .accounts(lu_client::accounts::LiquidUnstakeLst {
            pool: *unstake_pool_id,
            payer: wallet_keypair.pubkey(),
            user_transfer_authority: wallet_keypair.pubkey(),
            user_lst_account: wallet_lst_token_ata,
            sol_vault: unstake_pool_info.sol_vault,
            user_sol_account: wallet_keypair.pubkey(),
            manager_fee_account: unstake_pool_info.manager_fee_account,
            stake_pool: spl_stake_pool_address,
            stake_pool_validator_list: spl_stake_pool_state.validator_list,
            stake_pool_withdraw_authority,
            stake_pool_manager_fee_account: spl_stake_pool_state.manager_fee_account,
            stake_pool_mint: spl_stake_pool_state.pool_mint,
            token_program: spl_token::id(),
            stake_program: solana_sdk::stake::program::id(),
            stake_pool_program: *spl_stake_pool_program_id,
            system_program: solana_sdk::system_program::id(),
            clock: solana_sdk::sysvar::clock::id(),
            stake_history: solana_sdk::sysvar::stake_history::id(),
        })
        .accounts(vec![
            withdraw_stake_accounts
                .into_iter()
                .map(|x| AccountMeta::new(x, false))
                .collect_vec(),
            new_stake_accounts
                .iter()
                .map(|x| AccountMeta::new(x.pubkey(), !new_stake_account_as_pda))
                .collect_vec(),
            new_stake_pda_accounts
                .into_iter()
                .map(|x| AccountMeta::new(x, false))
                .collect_vec(),
        ]);

    instructions.extend(if new_stake_account_as_pda {
        builder
            .args(lu_client::args::LiquidUnstakeLstWithSeed {
                lst_amounts,
                minimum_lamports_out,
                stake_account_seed,
            })
            .instructions()?
    } else {
        builder
            .args(lu_client::args::LiquidUnstakeLst {
                lst_amounts,
                minimum_lamports_out,
            })
            .instructions()?
    });

    if !new_stake_account_as_pda {
        let create_instructions = new_stake_accounts
            .iter()
            .map(|stake_account_keypair| {
                create_account(
                    &wallet_keypair.pubkey(),
                    &stake_account_keypair.pubkey(),
                    solana_sdk::rent::Rent::default()
                        .minimum_balance(solana_sdk::stake::state::StakeStateV2::size_of()),
                    solana_sdk::stake::state::StakeStateV2::size_of() as u64,
                    &solana_sdk::stake::program::id(),
                )
            })
            .collect_vec();

        instructions.splice(0..0, create_instructions);
    }

    instructions.insert(
        0,
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_000_000),
    );

    let mut extra_signers = Vec::new();
    if !new_stake_account_as_pda {
        for stake_account in new_stake_accounts.iter() {
            if let PubkeyOrKeypair::Keypair(k) = stake_account {
                extra_signers.push(k);
            } else {
                return Err(anyhow!(
                    "expected keypair for new stake account when PDA option is disabled"
                ));
            }
        }
    }

    send_instructions(
        program,
        wallet_keypair,
        instructions,
        &extra_signers,
        simulate,
        Some(vec![
            wallet_keypair.pubkey(),
            unstake_pool_info.sol_vault,
            unstake_pool_info.manager_fee_account,
            new_stake_accounts[0].pubkey(),
        ]),
    )
    .await
}

async fn unstake_lst_wrapped(
    program: &ProgramClient,
    unstake_pool_id: &Pubkey,
    wallet_keypair: &Wallet,
    spl_stake_pool_program_id: &Pubkey,
    mint: &Pubkey,
    unstake_pool_info: &PoolAccount,
    amount: u64,
    minimum_lamports_out: Option<u64>,
    simulate: bool,
    new_stake_account_as_pda: bool,
) -> Result<()> {
    let rpc = program.rpc();
    let (spl_stake_pool_address, spl_stake_pool_state) =
        get_stake_pool_for_lst_mint(&rpc, mint, spl_stake_pool_program_id).await?;

    assert_eq!(spl_stake_pool_state.pool_mint, *mint);

    let spl_stake_pool_validator_list = rpc
        .get_account(&spl_stake_pool_state.validator_list)
        .await
        .map(|account| {
            let mut data = account.data.as_slice();
            spl_stake_pool::state::ValidatorList::deserialize(&mut data)
        })??;

    let stake_account_seed = unstake_pool_info.total_deactivating_stake;

    let (lst_amounts, withdraw_stake_accounts, new_stake_accounts, new_stake_pda_accounts) =
        if new_stake_account_as_pda {
            get_unstake_accounts_with_new_stake_account_as_pda(
                spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                stake_account_seed,
                &wallet_keypair.pubkey(),
                amount,
            )?
        } else {
            get_unstake_accounts(
                spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                amount,
            )?
        };

    let lst_amounts = pad_lst_amounts(lst_amounts)?;

    let wallet_lst_token_ata =
        token_account_address(&wallet_keypair.pubkey(), &spl_stake_pool_state.pool_mint);
    let wallet_wsol_token_ata =
        token_account_address(&wallet_keypair.pubkey(), &spl_token::native_mint::id());

    let stake_pool_withdraw_authority = Pubkey::find_program_address(
        &[&spl_stake_pool_address.to_bytes(), b"withdraw"],
        spl_stake_pool_program_id,
    )
    .0;

    let mut instructions = vec![
        create_ata_idempotent_ix(&wallet_keypair.pubkey(), &wallet_keypair.pubkey(), mint),
        create_ata_idempotent_ix(
            &wallet_keypair.pubkey(),
            &wallet_keypair.pubkey(),
            &spl_token::native_mint::id(),
        ),
    ];

    let builder = program
        .request()
        .accounts(lu_client::accounts::LiquidUnstakeLstWithWrapped {
            pool: *unstake_pool_id,
            payer: wallet_keypair.pubkey(),
            user_transfer_authority: wallet_keypair.pubkey(),
            user_lst_account: wallet_lst_token_ata,
            sol_vault: unstake_pool_info.sol_vault,
            user_sol_account: wallet_wsol_token_ata,
            manager_fee_account: unstake_pool_info.manager_fee_account,
            stake_pool: spl_stake_pool_address,
            stake_pool_validator_list: spl_stake_pool_state.validator_list,
            stake_pool_withdraw_authority,
            stake_pool_manager_fee_account: spl_stake_pool_state.manager_fee_account,
            stake_pool_mint: spl_stake_pool_state.pool_mint,
            token_program: spl_token::id(),
            stake_program: solana_sdk::stake::program::id(),
            stake_pool_program: *spl_stake_pool_program_id,
            system_program: solana_sdk::system_program::id(),
            clock: solana_sdk::sysvar::clock::id(),
        })
        .accounts(vec![
            withdraw_stake_accounts
                .into_iter()
                .map(|x| AccountMeta::new(x, false))
                .collect_vec(),
            new_stake_accounts
                .iter()
                .map(|x| AccountMeta::new(x.pubkey(), !new_stake_account_as_pda))
                .collect_vec(),
            new_stake_pda_accounts
                .into_iter()
                .map(|x| AccountMeta::new(x, false))
                .collect_vec(),
        ]);

    instructions.extend(if new_stake_account_as_pda {
        builder
            .args(lu_client::args::LiquidUnstakeLstWithWrappedSeed {
                lst_amounts,
                minimum_lamports_out,
                stake_account_seed,
            })
            .instructions()?
    } else {
        builder
            .args(lu_client::args::LiquidUnstakeLstWithWrapped {
                lst_amounts,
                minimum_lamports_out,
            })
            .instructions()?
    });

    if !new_stake_account_as_pda {
        let create_instructions = new_stake_accounts
            .iter()
            .map(|stake_account_keypair| {
                create_account(
                    &wallet_keypair.pubkey(),
                    &stake_account_keypair.pubkey(),
                    solana_sdk::rent::Rent::default()
                        .minimum_balance(solana_sdk::stake::state::StakeStateV2::size_of()),
                    solana_sdk::stake::state::StakeStateV2::size_of() as u64,
                    &solana_sdk::stake::program::id(),
                )
            })
            .collect_vec();

        instructions.splice(0..0, create_instructions);
    }

    instructions.insert(
        0,
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_000_000),
    );

    let mut extra_signers = Vec::new();
    if !new_stake_account_as_pda {
        for stake_account in new_stake_accounts.iter() {
            if let PubkeyOrKeypair::Keypair(k) = stake_account {
                extra_signers.push(k);
            } else {
                return Err(anyhow!(
                    "expected keypair for new stake account when PDA option is disabled"
                ));
            }
        }
    }

    send_instructions(
        program,
        wallet_keypair,
        instructions,
        &extra_signers,
        simulate,
        Some(vec![
            wallet_keypair.pubkey(),
            wallet_wsol_token_ata,
            unstake_pool_info.sol_vault,
            unstake_pool_info.manager_fee_account,
            new_stake_accounts[0].pubkey(),
        ]),
    )
    .await
}

async fn send_instructions(
    program: &ProgramClient,
    payer: &Wallet,
    instructions: Vec<Instruction>,
    extra_signers: &[&Keypair],
    simulate: bool,
    simulation_accounts_of_interest: Option<Vec<Pubkey>>,
) -> Result<()> {
    let recent_blockhash = program.rpc().get_latest_blockhash().await?;
    let payer_pubkey = payer.pubkey();
    if DUMP_TRANSACTION_MESSAGE.load(Ordering::Relaxed) {
        let message =
            Message::new_with_blockhash(&instructions, Some(&payer_pubkey), &recent_blockhash);
        let tx = Transaction::new_unsigned(message);
        println!("{}", encode_transaction_message(&tx));
        return Ok(());
    }

    let mut signers = vec![payer.keypair("sending or simulating a transaction")?];
    signers.extend_from_slice(extra_signers);
    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer_pubkey),
        &signers,
        recent_blockhash,
    );
    send_or_simulate_transaction(
        &program.rpc(),
        &tx,
        simulate,
        simulation_accounts_of_interest,
    )
    .await
}

fn encode_transaction_message(tx: &Transaction) -> String {
    bs58::encode(tx.message_data()).into_string()
}

async fn get_stake_pool_for_mint_from_supported_programs(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<(Pubkey, (Pubkey, StakePool))> {
    let spl_stake_pool_program_id = get_stake_pool_program_for_lst_mint(rpc, mint)
        .await?
        .ok_or_else(|| anyhow!("could not find a supported stake pool for mint {mint}"))?;
    let stake_pool = get_stake_pool_for_lst_mint(rpc, mint, &spl_stake_pool_program_id).await?;
    Ok((spl_stake_pool_program_id, stake_pool))
}

async fn get_stake_pool_program_for_lst_mint(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<Option<Pubkey>> {
    for program_id in SUPPORTED_STAKE_POOL_PROGRAMS {
        let mints_for_program = get_stake_pool_mints(rpc, &program_id)
            .await?
            .into_iter()
            .map(|(_pool_pubkey, _program_id, pool_mint)| pool_mint)
            .collect::<HashSet<Pubkey>>();

        if mints_for_program.contains(mint) {
            return Ok(Some(program_id));
        }
    }

    Ok(None)
}

async fn get_stake_pool_mints(
    rpc: &RpcClient,
    program_id: &Pubkey,
) -> Result<Vec<(Pubkey, Pubkey, Pubkey)>> {
    let spl_stake_pools = rpc
        .get_program_accounts_with_config(
            program_id,
            RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..RpcAccountInfoConfig::default()
                },
                filters: Some(vec![RpcFilterType::DataSize(611)]),
                ..RpcProgramAccountsConfig::default()
            },
        )
        .await?
        .into_iter()
        .filter_map(|(pubkey, account)| {
            let mut data = account.data.as_slice();
            spl_stake_pool::state::StakePool::deserialize(&mut data)
                .ok()
                .map(|pool_state| (pubkey, *program_id, pool_state.pool_mint))
        })
        .collect::<Vec<_>>();

    Ok(spl_stake_pools)
}

async fn send_or_simulate_transaction(
    rpc: &RpcClient,
    tx: &Transaction,
    simulate: bool,
    simulation_accounts_of_interest: Option<Vec<Pubkey>>,
) -> Result<()> {
    if simulate {
        let simulation_accounts_of_interest = simulation_accounts_of_interest.unwrap_or_default();
        let mut pre_simulation_accounts_of_interest = vec![];

        for account in simulation_accounts_of_interest.iter() {
            let pre_account = fetch_optional_account(rpc, account).await?;
            pre_simulation_accounts_of_interest.push((
                account,
                pre_account.as_ref().map(|a| a.lamports).unwrap_or(0),
                pre_account.as_ref().and_then(token_amount_from_account),
            ));
        }

        let result = rpc
            .simulate_transaction_with_config(
                tx,
                RpcSimulateTransactionConfig {
                    accounts: Some(RpcSimulateTransactionAccountsConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        addresses: simulation_accounts_of_interest
                            .iter()
                            .map(|p| p.to_string())
                            .collect_vec(),
                    }),
                    ..Default::default()
                },
            )
            .await?;

        if let Some(ref err) = result.value.err {
            println!("Simulation failed: {:#?}", result.value);
            return Err(anyhow!("simulation failed: {err:?}"));
        }

        println!("Simulation success");
        if let Some(accounts) = result.value.accounts {
            for (account_pre_simulation, account_post_simulation) in
                izip!(pre_simulation_accounts_of_interest, accounts)
            {
                let (account_key, pre_lamports, pre_token_amount) = account_pre_simulation;
                if let Some(account_post_simulation) = account_post_simulation {
                    let post_lamports = account_post_simulation.lamports;
                    println!(
                        "Account {} lamports {} -> {} diff {}",
                        account_key,
                        pre_lamports,
                        post_lamports,
                        (post_lamports as i64) - (pre_lamports as i64)
                    );
                    if let Some(post_token_amount) =
                        token_amount_from_ui_account(&account_post_simulation)
                    {
                        let pre_token_amount = pre_token_amount.unwrap_or(0);
                        println!(
                            "Account {} token_amount {} -> {} diff {}",
                            account_key,
                            pre_token_amount,
                            post_token_amount,
                            (post_token_amount as i128) - (pre_token_amount as i128)
                        );
                    }
                } else {
                    println!(
                        "Account {} lamports {} -> 0 diff {}",
                        account_key,
                        pre_lamports,
                        -(pre_lamports as i128)
                    );
                    if let Some(pre_token_amount) = pre_token_amount {
                        println!(
                            "Account {} token_amount {} -> 0 diff {}",
                            account_key,
                            pre_token_amount,
                            -(pre_token_amount as i128)
                        );
                    }
                }
            }
        }
    } else {
        let signature = rpc.send_and_confirm_transaction(tx).await?;
        println!("Signature: {signature}");
    }
    Ok(())
}

async fn simulate_account_deltas(
    rpc: &RpcClient,
    tx: &Transaction,
    accounts_of_interest: &[Pubkey],
) -> Result<Vec<AccountSimulationDelta>> {
    let mut pre_accounts = Vec::with_capacity(accounts_of_interest.len());
    for account in accounts_of_interest {
        let pre_account = fetch_optional_account(rpc, account).await?;
        pre_accounts.push((
            pre_account
                .as_ref()
                .map(|account| account.lamports)
                .unwrap_or(0),
            pre_account.as_ref().and_then(token_amount_from_account),
        ));
    }

    let result = rpc
        .simulate_transaction_with_config(
            tx,
            RpcSimulateTransactionConfig {
                accounts: Some(RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: accounts_of_interest
                        .iter()
                        .map(|p| p.to_string())
                        .collect_vec(),
                }),
                ..Default::default()
            },
        )
        .await?;

    if let Some(ref err) = result.value.err {
        return Err(anyhow!("quote simulation failed: {err:?}"));
    }

    let post_accounts = result
        .value
        .accounts
        .ok_or_else(|| anyhow!("quote simulation did not return requested accounts"))?;
    if post_accounts.len() != accounts_of_interest.len() {
        return Err(anyhow!(
            "quote simulation returned {} accounts, expected {}",
            post_accounts.len(),
            accounts_of_interest.len()
        ));
    }

    Ok(pre_accounts
        .into_iter()
        .zip(post_accounts)
        .map(|((pre_balance, pre_token_amount), post_account)| {
            let post_token_amount = post_account.as_ref().and_then(token_amount_from_ui_account);
            AccountSimulationDelta {
                pre_balance,
                post_balance: post_account.map(|account| account.lamports).unwrap_or(0),
                pre_token_amount,
                post_token_amount,
            }
        })
        .collect())
}

async fn get_stake_pool_for_lst_mint(
    rpc: &RpcClient,
    mint: &Pubkey,
    spl_stake_pool_program_id: &Pubkey,
) -> Result<(Pubkey, spl_stake_pool::state::StakePool)> {
    let mut spl_stake_pools = rpc
        .get_program_accounts_with_config(
            spl_stake_pool_program_id,
            RpcProgramAccountsConfig {
                filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                    offset_of!(spl_stake_pool::state::StakePool, pool_mint),
                    &mint.to_bytes(),
                ))]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..RpcAccountInfoConfig::default()
                },
                ..RpcProgramAccountsConfig::default()
            },
        )
        .await?
        .into_iter()
        .map(|(pubkey, account)| {
            let mut data = account.data.as_slice();
            let pool_state = spl_stake_pool::state::StakePool::deserialize(&mut data).unwrap();

            (pubkey, pool_state)
        })
        .collect::<Vec<_>>();

    if spl_stake_pools.len() != 1 {
        return Err(anyhow!(
            "found {} stake pools for mint {}",
            spl_stake_pools.len(),
            mint
        ));
    }

    Ok(spl_stake_pools.pop().unwrap())
}

pub fn quote_lst_unstake(
    stake_pool_state: &StakePool,
    liquid_unstake_pool_state: &PoolAccount,
    pool_tokens: u64,
) -> Result<i64> {
    let pool_tokens_fee = stake_pool_state
        .calc_pool_tokens_stake_withdrawal_fee(pool_tokens)
        .unwrap();
    let pool_tokens_net = pool_tokens - pool_tokens_fee;
    let total_amount_to_unstake = stake_pool_state
        .calc_lamports_withdraw_amount(pool_tokens_net)
        .unwrap();

    let stake_account_rent = solana_sdk::rent::Rent::default()
        .minimum_balance(size_of::<solana_sdk::stake::state::StakeStateV2>());

    let total_amount_to_unstake = total_amount_to_unstake + stake_account_rent;

    if total_amount_to_unstake > liquid_unstake_pool_state.sol_vault_lamports {
        return Err(anyhow!(
            "not enough liquidity in the unstake pool to cover this unstake amount"
        ));
    }

    let base_fee_pct_bps = Fee::calculate_base_fee(
        liquid_unstake_pool_state,
        liquid_unstake_pool_state.sol_vault_lamports,
        total_amount_to_unstake,
    )? as u128;

    let fee = Fee {
        base_fee: base_fee_pct_bps
            .mul(total_amount_to_unstake as u128)
            .div(FEE_PCT_DIVISOR as u128) as u64,
        manager_fee: base_fee_pct_bps
            .mul(total_amount_to_unstake as u128)
            .mul(liquid_unstake_pool_state.manager_fee_pct as u128)
            .div(100_u128 * FEE_PCT_DIVISOR as u128) as u64,
    };

    let fee_amount = fee.total_fee();
    let amount_out = total_amount_to_unstake as i64 - fee_amount as i64 - stake_account_rent as i64;

    Ok(amount_out)
}

pub fn quote_lst_unstake_wrapped(
    stake_pool_state: &StakePool,
    liquid_unstake_pool_state: &PoolAccount,
    pool_tokens: u64,
    new_stake_account_as_pda: bool,
) -> Result<(i64, i64)> {
    let pool_tokens_fee = stake_pool_state
        .calc_pool_tokens_stake_withdrawal_fee(pool_tokens)
        .unwrap();
    let pool_tokens_net = pool_tokens - pool_tokens_fee;
    let total_amount_to_unstake = stake_pool_state
        .calc_lamports_withdraw_amount(pool_tokens_net)
        .unwrap();

    let stake_account_rent = solana_sdk::rent::Rent::default()
        .minimum_balance(size_of::<solana_sdk::stake::state::StakeStateV2>());

    let total_amount_to_unstake = total_amount_to_unstake + stake_account_rent;

    if total_amount_to_unstake > liquid_unstake_pool_state.sol_vault_lamports {
        return Err(anyhow!(
            "not enough liquidity in the unstake pool to cover this unstake amount"
        ));
    }

    let base_fee_pct_bps = Fee::calculate_base_fee(
        liquid_unstake_pool_state,
        liquid_unstake_pool_state.sol_vault_lamports,
        total_amount_to_unstake,
    )? as u128;

    let fee = Fee {
        base_fee: base_fee_pct_bps
            .mul(total_amount_to_unstake as u128)
            .div(FEE_PCT_DIVISOR as u128) as u64,
        manager_fee: base_fee_pct_bps
            .mul(total_amount_to_unstake as u128)
            .mul(liquid_unstake_pool_state.manager_fee_pct as u128)
            .div(100_u128 * FEE_PCT_DIVISOR as u128) as u64,
    };

    let fee_amount = fee.total_fee();
    let wsol_amount_out =
        total_amount_to_unstake as i64 - fee_amount as i64 - stake_account_rent as i64;
    let wsol_amount_out_extra_if_user_paid_for_new_stake_account = if !new_stake_account_as_pda {
        stake_account_rent as i64
    } else {
        0
    };

    Ok((
        wsol_amount_out + wsol_amount_out_extra_if_user_paid_for_new_stake_account,
        0,
    ))
}

async fn quote_sell_lst(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    mint: &Pubkey,
    lst_amount: u64,
) -> Result<SellQuote> {
    require_lst_info_for_trade(rpc, pool_id, mint).await?;
    let (_, (_, stake_pool_state)) =
        get_stake_pool_for_mint_from_supported_programs(rpc, mint).await?;
    validate_stake_pool_for_v3_quote(rpc, &stake_pool_state).await?;
    check_inventory_summary_ready_for_trade(rpc, pool_id).await?;
    check_lst_rate_drift(rpc, pool_id, pool, mint, &stake_pool_state).await?;
    check_epoch_progress(rpc, pool.max_epoch_progress_pct).await?;

    let epoch_progress = get_epoch_progress(rpc).await?;
    calculate_sell_quote(pool, &stake_pool_state, lst_amount, epoch_progress)
}

fn calculate_sell_quote(
    pool: &PoolAccount,
    stake_pool_state: &StakePool,
    lst_amount: u64,
    epoch_progress: u64,
) -> Result<SellQuote> {
    let total_sol_value = calculate_lst_sol_value(
        lst_amount,
        stake_pool_state.total_lamports,
        stake_pool_state.pool_token_supply,
        epoch_progress,
        pool.expected_inflation_per_epoch,
    )?;

    if pool.sol_vault_lamports < total_sol_value {
        return Err(anyhow!("pool SOL vault accounting is below LST value"));
    }

    let base_fee_pct =
        Fee::calculate_base_fee(pool, pool.sol_vault_lamports, total_sol_value)? as u64;
    let base_flat_fee_pct = base_fee_pct
        .checked_add(pool.sell_lst_flat_fee as u64)
        .ok_or_else(|| anyhow!("fee overflow"))?;
    let base_flat_fee = (base_flat_fee_pct as u128)
        .checked_mul(total_sol_value as u128)
        .ok_or_else(|| anyhow!("fee overflow"))?
        .checked_div(FEE_PCT_DIVISOR as u128)
        .ok_or_else(|| anyhow!("fee underflow"))? as u64;
    let stake_pool_fee = ceil_fee(
        total_sol_value,
        stake_pool_state.stake_withdrawal_fee.numerator,
        stake_pool_state.stake_withdrawal_fee.denominator,
    )?;
    let total_fee = base_flat_fee
        .checked_add(stake_pool_fee)
        .ok_or_else(|| anyhow!("fee overflow"))?;
    let manager_fee = (total_fee as u128)
        .checked_mul(pool.manager_fee_pct as u128)
        .ok_or_else(|| anyhow!("fee overflow"))?
        .checked_div(100)
        .ok_or_else(|| anyhow!("fee underflow"))? as u64;
    let pool_fee = total_fee
        .checked_sub(manager_fee)
        .ok_or_else(|| anyhow!("fee underflow"))?;
    let amount_to_user = total_sol_value
        .checked_sub(total_fee)
        .ok_or_else(|| anyhow!("fee exceeds LST value"))?;
    let total_vault_deduction = amount_to_user
        .checked_add(manager_fee)
        .ok_or_else(|| anyhow!("fee overflow"))?;

    if pool.sol_vault_lamports < total_vault_deduction {
        return Err(anyhow!(
            "pool SOL vault cannot cover user payout plus manager fee"
        ));
    }

    Ok(SellQuote {
        total_sol_value,
        base_fee_pct,
        base_flat_fee,
        stake_pool_fee,
        total_fee,
        manager_fee,
        pool_fee,
        amount_to_user,
    })
}

fn calculate_protocol_buy_quote(
    pool: &PoolAccount,
    stake_pool_state: &StakePool,
    lst_amount: u64,
    epoch_progress: u64,
) -> Result<ProtocolBuyQuote> {
    let multiplier =
        calculate_inflation_multiplier(epoch_progress, pool.expected_inflation_per_epoch)?;
    let total_sol_value_without_discount = calculate_lst_sol_value(
        lst_amount,
        stake_pool_state.total_lamports,
        stake_pool_state.pool_token_supply,
        epoch_progress,
        pool.expected_inflation_per_epoch,
    )?;
    let half_stake_pool_fee_pct = if stake_pool_state.stake_withdrawal_fee.denominator > 0 {
        (stake_pool_state.stake_withdrawal_fee.numerator as u128)
            .checked_mul(FEE_PCT_DIVISOR as u128)
            .ok_or_else(|| anyhow!("fee overflow"))?
            .checked_div(stake_pool_state.stake_withdrawal_fee.denominator as u128)
            .ok_or_else(|| anyhow!("fee underflow"))?
            .checked_div(2)
            .ok_or_else(|| anyhow!("fee underflow"))? as u64
    } else {
        0
    };
    let price_improvement_multiplier = (FEE_PCT_DIVISOR as u64)
        .checked_sub(half_stake_pool_fee_pct)
        .ok_or_else(|| anyhow!("stake pool fee exceeds 100%"))?;
    let lst_cost = (lst_amount as u128)
        .checked_mul(stake_pool_state.total_lamports as u128)
        .ok_or_else(|| anyhow!("price overflow"))?
        .checked_mul(multiplier as u128)
        .ok_or_else(|| anyhow!("price overflow"))?
        .checked_div(stake_pool_state.pool_token_supply as u128)
        .ok_or_else(|| anyhow!("price underflow"))?
        .checked_mul(price_improvement_multiplier as u128)
        .ok_or_else(|| anyhow!("price overflow"))?
        .checked_div(INFLATION_PCT_DIVISOR as u128)
        .ok_or_else(|| anyhow!("price underflow"))?
        .checked_div(FEE_PCT_DIVISOR as u128)
        .ok_or_else(|| anyhow!("price underflow"))? as u64;

    let dynamic_fee_pct = calculate_dynamic_fee(pool)?;
    let total_fee = (lst_cost as u128)
        .checked_mul(dynamic_fee_pct as u128)
        .ok_or_else(|| anyhow!("fee overflow"))?
        .checked_div(FEE_PCT_DIVISOR as u128)
        .ok_or_else(|| anyhow!("fee underflow"))? as u64;
    let manager_fee = (total_fee as u128)
        .checked_mul(pool.manager_fee_pct as u128)
        .ok_or_else(|| anyhow!("fee overflow"))?
        .checked_div(100)
        .ok_or_else(|| anyhow!("fee underflow"))? as u64;
    let pool_fee = total_fee
        .checked_sub(manager_fee)
        .ok_or_else(|| anyhow!("fee underflow"))?;
    let total_cost = lst_cost
        .checked_add(total_fee)
        .ok_or_else(|| anyhow!("cost overflow"))?;

    if pool.min_buy_lamports > 0 && total_cost < pool.min_buy_lamports {
        return Err(anyhow!(
            "buy total cost {} is below pool.min_buy_lamports {}",
            total_cost,
            pool.min_buy_lamports
        ));
    }

    Ok(ProtocolBuyQuote {
        total_sol_value_without_discount,
        half_stake_pool_fee_pct,
        lst_cost,
        dynamic_fee_pct,
        pool_fee,
        manager_fee,
        total_fee,
        total_cost,
    })
}

async fn quote_buy_lst(
    program: &ProgramClient,
    pool_id: &Pubkey,
    wallet: &Keypair,
    pool: &PoolAccount,
    mint: &Pubkey,
    lst_amount: u64,
) -> Result<BuyQuote> {
    let rpc = program.rpc();
    require_lst_info_for_trade(&rpc, pool_id, mint).await?;
    let (_, (stake_pool_address, stake_pool_state)) =
        get_stake_pool_for_mint_from_supported_programs(&rpc, mint).await?;
    validate_stake_pool_for_v3_quote(&rpc, &stake_pool_state).await?;
    check_lst_rate_drift(&rpc, pool_id, pool, mint, &stake_pool_state).await?;
    check_epoch_progress(&rpc, pool.max_epoch_progress_pct).await?;

    let pool_lst_token_account = token_account_address(pool_id, mint);
    let pool_lst_amount = get_token_account_amount(&rpc, &pool_lst_token_account)
        .await?
        .unwrap_or(0);
    if pool_lst_amount < lst_amount {
        return Err(anyhow!(
            "pool owns {} tokens for mint {}, below requested {}",
            pool_lst_amount,
            mint,
            lst_amount
        ));
    }

    let epoch_progress = get_epoch_progress(&rpc).await?;
    let protocol_quote =
        calculate_protocol_buy_quote(pool, &stake_pool_state, lst_amount, epoch_progress)?;

    let token_account_rent = rpc
        .get_minimum_balance_for_rent_exemption(spl_token::state::Account::LEN)
        .await?;
    let user_wsol_account = token_account_address(&wallet.pubkey(), &spl_token::native_mint::id());
    let user_wsol_account_rent = if fetch_optional_account(&rpc, &user_wsol_account)
        .await?
        .is_some()
    {
        0
    } else {
        token_account_rent
    };
    let user_lst_account = token_account_address(&wallet.pubkey(), mint);
    let user_lst_account_rent = if fetch_optional_account(&rpc, &user_lst_account)
        .await?
        .is_some()
    {
        0
    } else {
        token_account_rent
    };

    let buy_instructions = buy_lst_instructions(
        program,
        pool_id,
        &wallet.pubkey(),
        pool,
        mint,
        &stake_pool_address,
        lst_amount,
        None,
    )?;
    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let message = Message::new_with_blockhash(
        &buy_instructions.instructions,
        Some(&wallet.pubkey()),
        &recent_blockhash,
    );
    let estimated_transaction_fee = rpc.get_fee_for_message(&message).await?;
    let tx = Transaction::new_signed_with_payer(
        &buy_instructions.instructions,
        Some(&wallet.pubkey()),
        &[wallet],
        recent_blockhash,
    );
    let simulation_accounts = vec![
        wallet.pubkey(),
        buy_instructions.user_wsol_account,
        buy_instructions.user_lst_account,
        buy_instructions.pool_lst_token_account,
        pool.sol_vault,
        pool.manager_fee_account,
        buy_instructions.wsol_buffer_account,
    ];
    let simulation_deltas = simulate_account_deltas(&rpc, &tx, &simulation_accounts).await?;
    let wallet_delta = &simulation_deltas[0];
    let user_wsol_delta = &simulation_deltas[1];
    let user_lst_delta = &simulation_deltas[2];
    let vault_delta = &simulation_deltas[4];
    let manager_delta = &simulation_deltas[5];
    let simulated_wallet_lamports_out = wallet_delta
        .pre_balance
        .checked_sub(wallet_delta.post_balance)
        .ok_or_else(|| anyhow!("quote simulation increased wallet lamports"))?;
    let simulated_user_wsol_amount_in = user_wsol_delta
        .pre_token_amount
        .unwrap_or(0)
        .checked_sub(user_wsol_delta.post_token_amount.unwrap_or(0))
        .ok_or_else(|| anyhow!("quote simulation increased user WSOL amount"))?;
    let simulated_vault_in = vault_delta
        .post_balance
        .checked_sub(vault_delta.pre_balance)
        .ok_or_else(|| anyhow!("quote simulation decreased SOL vault lamports"))?;
    let simulated_manager_in = manager_delta
        .post_balance
        .checked_sub(manager_delta.pre_balance)
        .ok_or_else(|| anyhow!("quote simulation decreased manager fee account lamports"))?;
    let simulated_protocol_total_cost = simulated_vault_in
        .checked_add(simulated_manager_in)
        .ok_or_else(|| anyhow!("simulated protocol cost overflow"))?;
    let simulated_user_lst_amount_out = user_lst_delta
        .post_token_amount
        .unwrap_or(0)
        .checked_sub(user_lst_delta.pre_token_amount.unwrap_or(0))
        .ok_or_else(|| anyhow!("quote simulation decreased user LST amount"))?;
    let estimated_wallet_lamports_out = user_wsol_account_rent
        .checked_add(user_lst_account_rent)
        .ok_or_else(|| anyhow!("wallet debit overflow"))?
        .checked_add(estimated_transaction_fee)
        .ok_or_else(|| anyhow!("wallet debit overflow"))?;

    check_inventory_summary_can_cover_buy(
        &rpc,
        pool_id,
        pool,
        mint,
        &stake_pool_address,
        &stake_pool_state,
        pool_lst_amount,
        lst_amount,
    )
    .await?;

    Ok(BuyQuote {
        total_sol_value_without_discount: protocol_quote.total_sol_value_without_discount,
        half_stake_pool_fee_pct: protocol_quote.half_stake_pool_fee_pct,
        lst_cost: protocol_quote.lst_cost,
        dynamic_fee_pct: protocol_quote.dynamic_fee_pct,
        pool_fee: protocol_quote.pool_fee,
        manager_fee: protocol_quote.manager_fee,
        total_fee: protocol_quote.total_fee,
        total_cost: protocol_quote.total_cost,
        user_wsol_account_rent,
        user_lst_account_rent,
        estimated_transaction_fee,
        estimated_wallet_lamports_out,
        simulated_protocol_total_cost,
        simulated_wallet_lamports_out,
        simulated_user_wsol_amount_in,
        simulated_user_lst_amount_out,
    })
}

fn print_sell_quote(amount: u64, mint: Pubkey, quote: &SellQuote) {
    println!("Sell quote for {amount} {mint}");
    println!("WSOL lamports received: {}", quote.amount_to_user);
    println!("LST tokens sold: {amount}");
    println!();
    println!("Details:");
    println!("  total_sol_value: {}", quote.total_sol_value);
    println!("  base_fee_pct: {}", quote.base_fee_pct);
    println!("  base_plus_flat_fee: {}", quote.base_flat_fee);
    println!("  stake_pool_fee: {}", quote.stake_pool_fee);
    println!("  total_fee: {}", quote.total_fee);
    println!("  pool_fee: {}", quote.pool_fee);
    println!("  manager_fee: {}", quote.manager_fee);
    println!("  amount_to_user_wsol: {}", quote.amount_to_user);
}

fn print_buy_quote(amount: u64, mint: Pubkey, quote: &BuyQuote) {
    println!("Buy quote for {amount} {mint}");
    println!("cost lamports: {}", quote.simulated_user_wsol_amount_in);
    println!(
        "LST tokens received: {}",
        quote.simulated_user_lst_amount_out
    );
    println!();
    println!("Details:");
    println!(
        "  total_sol_value_without_discount: {}",
        quote.total_sol_value_without_discount
    );
    println!(
        "  half_stake_pool_fee_pct: {}",
        quote.half_stake_pool_fee_pct
    );
    println!("  lst_cost_before_fees: {}", quote.lst_cost);
    println!("  dynamic_fee_pct: {}", quote.dynamic_fee_pct);
    println!("  pool_fee: {}", quote.pool_fee);
    println!("  manager_fee: {}", quote.manager_fee);
    println!("  total_fee: {}", quote.total_fee);
    println!(
        "  formula_protocol_total_cost_lamports: {}",
        quote.total_cost
    );
    println!(
        "  user_lst_account_rent_lamports: {}",
        quote.user_lst_account_rent
    );
    println!(
        "  user_wsol_account_rent_lamports: {}",
        quote.user_wsol_account_rent
    );
    println!(
        "  estimated_transaction_fee_lamports: {}",
        quote.estimated_transaction_fee
    );
    println!(
        "  estimated_wallet_lamports_out: {}",
        quote.estimated_wallet_lamports_out
    );
    println!(
        "  simulated_protocol_total_cost_lamports: {}",
        quote.simulated_protocol_total_cost
    );
    println!(
        "  simulated_wallet_lamports_out: {}",
        quote.simulated_wallet_lamports_out
    );
    println!(
        "  simulated_user_wsol_amount_in: {}",
        quote.simulated_user_wsol_amount_in
    );
    println!(
        "  simulated_user_lst_amount_out: {}",
        quote.simulated_user_lst_amount_out
    );
}

fn calculate_dynamic_fee(pool: &PoolAccount) -> Result<u32> {
    if pool.activity_accumulator == 0 {
        return Ok(pool.buy_lst_flat_fee);
    }

    let activity_ratio = (pool.activity_accumulator as u128)
        .checked_mul(10_000)
        .ok_or_else(|| anyhow!("activity overflow"))?
        .checked_div(BUY_REFERENCE_INVENTORY as u128)
        .ok_or_else(|| anyhow!("activity underflow"))? as u64;
    let exponential_term = (activity_ratio as u128)
        .checked_mul(activity_ratio as u128)
        .ok_or_else(|| anyhow!("activity overflow"))?
        .checked_div(BUY_ACTIVITY_SCALING as u128)
        .ok_or_else(|| anyhow!("activity underflow"))? as u64;
    let total_fee = (pool.buy_lst_flat_fee as u64)
        .checked_add(exponential_term)
        .ok_or_else(|| anyhow!("fee overflow"))?
        .min(pool.buy_lst_dynamic_fee_max as u64);

    Ok(total_fee as u32)
}

async fn require_lst_info_for_trade(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    mint: &Pubkey,
) -> Result<LstInfoAccount> {
    let lst_info_address = lst_info_address(pool_id, mint);
    let lst_info = fetch_anchor_account::<LstInfoAccount>(rpc, &lst_info_address)
        .await?
        .ok_or_else(|| anyhow!("missing LstInfo {lst_info_address}; run upsert-lst-info"))?;
    if !lst_info.enabled {
        return Err(anyhow!("LST {mint} is disabled for pool {pool_id}"));
    }
    Ok(lst_info)
}

async fn check_inventory_summary_ready_for_trade(rpc: &RpcClient, pool_id: &Pubkey) -> Result<()> {
    let summary_address = inventory_summary_address(pool_id);
    let Some(summary) =
        fetch_anchor_account::<InventorySummaryAccount>(rpc, &summary_address).await?
    else {
        return Ok(());
    };

    if summary.sync_in_progress {
        return Err(anyhow!(
            "inventory sync is in progress for {summary_address}; finish or abort sync-inventory before trading"
        ));
    }

    let epoch_info = rpc.get_epoch_info().await?;
    if summary.snapshot_epoch != epoch_info.epoch {
        return Err(anyhow!(
            "inventory summary is from epoch {}, current epoch {}; run sync-inventory",
            summary.snapshot_epoch,
            epoch_info.epoch
        ));
    }

    Ok(())
}

async fn check_lst_rate_drift(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    mint: &Pubkey,
    stake_pool: &StakePool,
) -> Result<()> {
    let lst_info = require_lst_info_for_trade(rpc, pool_id, mint).await?;
    check_lst_rate_drift_for_info(pool, &lst_info, stake_pool)
}

fn check_lst_rate_drift_for_info(
    pool: &PoolAccount,
    lst_info: &LstInfoAccount,
    stake_pool: &StakePool,
) -> Result<()> {
    let current_rate = calculate_stake_pool_rate(stake_pool)?;
    if rate_drift_exceeds(lst_info, current_rate, pool.max_rate_drift_bps) {
        return Err(anyhow!(
            "current LST exchange rate exceeds configured drift cap; run sync-inventory or inspect the stake pool"
        ));
    }

    Ok(())
}

fn rate_drift_exceeds(lst_info: &LstInfoAccount, current_rate: u64, max_drift_bps: u16) -> bool {
    let mut last_rate = None;
    let mut last_epoch = 0;
    for i in 0..lst_info.rate_history_len as usize {
        if last_rate.is_none() || lst_info.rate_history_epochs[i] > last_epoch {
            last_epoch = lst_info.rate_history_epochs[i];
            last_rate = Some(lst_info.rate_history_rates[i]);
        }
    }

    let Some(last_rate) = last_rate else {
        return false;
    };
    if last_rate == 0 {
        return false;
    }

    let abs_diff = current_rate.abs_diff(last_rate);
    (abs_diff as u128).saturating_mul(FEE_PCT_DIVISOR as u128)
        > (last_rate as u128).saturating_mul(max_drift_bps as u128)
}

async fn check_inventory_summary_can_cover_buy(
    rpc: &RpcClient,
    pool_id: &Pubkey,
    pool: &PoolAccount,
    _mint: &Pubkey,
    stake_pool_address: &Pubkey,
    stake_pool_state: &StakePool,
    pool_lst_amount_before: u64,
    buy_lst_amount: u64,
) -> Result<()> {
    let summary_address = inventory_summary_address(pool_id);
    let summary = fetch_anchor_account::<InventorySummaryAccount>(rpc, &summary_address)
        .await?
        .ok_or_else(|| anyhow!("missing InventorySummary {summary_address}; run sync-inventory"))?;
    if summary.sync_in_progress {
        return Err(anyhow!(
            "inventory sync is in progress for {summary_address}; finish or abort sync-inventory before trading"
        ));
    }
    let epoch_info = rpc.get_epoch_info().await?;
    if summary.snapshot_epoch != epoch_info.epoch {
        return Err(anyhow!(
            "inventory summary is from epoch {}, current epoch {}; run sync-inventory",
            summary.snapshot_epoch,
            epoch_info.epoch
        ));
    }

    let value_before = calculate_lst_sol_value(
        pool_lst_amount_before,
        stake_pool_state.total_lamports,
        stake_pool_state.pool_token_supply,
        summary.snapshot_progress,
        pool.expected_inflation_per_epoch,
    )?;
    let value_after = calculate_lst_sol_value(
        pool_lst_amount_before
            .checked_sub(buy_lst_amount)
            .ok_or_else(|| anyhow!("pool LST balance below buy amount"))?,
        stake_pool_state.total_lamports,
        stake_pool_state.pool_token_supply,
        summary.snapshot_progress,
        pool.expected_inflation_per_epoch,
    )?;
    let delta = i128::from(value_after) - i128::from(value_before);
    let next = i128::from(summary.total_value_snapshot) + delta;
    if next < 0 {
        return Err(anyhow!(
            "inventory summary cannot cover buy for stake pool {}; run sync-inventory",
            stake_pool_address
        ));
    }

    Ok(())
}

async fn validate_stake_pool_for_v3_quote(rpc: &RpcClient, stake_pool: &StakePool) -> Result<()> {
    if stake_pool.pool_token_supply == 0 {
        return Err(anyhow!("stake pool token supply is zero"));
    }
    let epoch = rpc.get_epoch_info().await?.epoch;
    if stake_pool.last_update_epoch < epoch {
        return Err(anyhow!(
            "stake pool last_update_epoch {} is older than current epoch {}",
            stake_pool.last_update_epoch,
            epoch
        ));
    }
    Ok(())
}

async fn check_epoch_progress(rpc: &RpcClient, max_epoch_progress_pct: u8) -> Result<()> {
    if max_epoch_progress_pct > 100 {
        return Err(anyhow!("max_epoch_progress_pct cannot exceed 100"));
    }
    let epoch_progress = get_epoch_progress(rpc).await?;
    let threshold = ((u64::MAX as u128 * max_epoch_progress_pct as u128) / 100) as u64;
    if epoch_progress >= threshold {
        return Err(anyhow!(
            "epoch progress has reached configured max {}%",
            max_epoch_progress_pct
        ));
    }
    Ok(())
}

async fn get_epoch_progress(rpc: &RpcClient) -> Result<u64> {
    let epoch_info = rpc.get_epoch_info().await?;
    epoch_progress_from_slot_index(epoch_info.slot_index, epoch_info.slots_in_epoch)
}

fn epoch_progress_from_slot_index(slot_index: u64, slots_in_epoch: u64) -> Result<u64> {
    if slots_in_epoch == 0 {
        return Err(anyhow!("RPC returned zero slots_in_epoch"));
    }
    Ok(((slot_index as u128)
        .checked_mul(u64::MAX as u128)
        .ok_or_else(|| anyhow!("epoch progress overflow"))?
        .checked_div(slots_in_epoch as u128)
        .ok_or_else(|| anyhow!("epoch progress underflow"))?) as u64)
}

fn calculate_inflation_multiplier(
    epoch_progress: u64,
    expected_inflation_per_epoch: u32,
) -> Result<u64> {
    let progress_scaled = (epoch_progress as u128)
        .checked_mul(INFLATION_PCT_DIVISOR as u128)
        .ok_or_else(|| anyhow!("inflation overflow"))?
        .checked_div(u64::MAX as u128)
        .ok_or_else(|| anyhow!("inflation underflow"))? as u64;
    let inflation_addition = (progress_scaled as u128)
        .checked_mul(expected_inflation_per_epoch as u128)
        .ok_or_else(|| anyhow!("inflation overflow"))?
        .checked_div(FEE_PCT_DIVISOR as u128)
        .ok_or_else(|| anyhow!("inflation underflow"))? as u64;

    INFLATION_PCT_DIVISOR
        .checked_add(inflation_addition)
        .ok_or_else(|| anyhow!("inflation overflow"))
}

fn calculate_lst_sol_value(
    lst_amount: u64,
    total_lamports: u64,
    pool_token_supply: u64,
    epoch_progress: u64,
    expected_inflation_per_epoch: u32,
) -> Result<u64> {
    if pool_token_supply == 0 {
        return Err(anyhow!("stake pool token supply is zero"));
    }
    let multiplier = calculate_inflation_multiplier(epoch_progress, expected_inflation_per_epoch)?;
    Ok((lst_amount as u128)
        .checked_mul(total_lamports as u128)
        .ok_or_else(|| anyhow!("price overflow"))?
        .checked_mul(multiplier as u128)
        .ok_or_else(|| anyhow!("price overflow"))?
        .checked_div(pool_token_supply as u128)
        .ok_or_else(|| anyhow!("price underflow"))?
        .checked_div(INFLATION_PCT_DIVISOR as u128)
        .ok_or_else(|| anyhow!("price underflow"))? as u64)
}

fn ceil_fee(amount: u64, numerator: u64, denominator: u64) -> Result<u64> {
    if denominator == 0 {
        return Ok(0);
    }
    let denominator = denominator as u128;
    Ok((amount as u128)
        .checked_mul(numerator as u128)
        .ok_or_else(|| anyhow!("fee overflow"))?
        .checked_add(denominator)
        .ok_or_else(|| anyhow!("fee overflow"))?
        .checked_sub(1)
        .ok_or_else(|| anyhow!("fee underflow"))?
        .checked_div(denominator)
        .ok_or_else(|| anyhow!("fee underflow"))? as u64)
}

fn pad_lst_amounts(lst_amounts: Vec<u64>) -> Result<[u64; 5]> {
    if lst_amounts.is_empty() {
        return Err(anyhow!(
            "amount must select at least one validator stake account"
        ));
    }
    if lst_amounts.len() > 5 {
        return Err(anyhow!(
            "selected {} validator stake accounts, but the protocol supports at most 5 per transaction; reduce the amount",
            lst_amounts.len()
        ));
    }

    let mut padded = [0_u64; 5];
    for (i, amount) in lst_amounts.into_iter().enumerate() {
        padded[i] = amount;
    }
    Ok(padded)
}

async fn list_lst_infos(
    rpc: &RpcClient,
    pool_id: &Pubkey,
) -> Result<Vec<(Pubkey, LstInfoAccount)>> {
    let mut entries = Vec::new();
    for record in list_lst_info_records(rpc, pool_id).await? {
        if let Some(lst_info) = record.v3 {
            entries.push((record.address, lst_info));
        }
    }
    entries.sort_by_key(|(_, info)| info.mint);
    Ok(entries)
}

async fn list_pool_lst_balances(rpc: &RpcClient, pool_id: &Pubkey) -> Result<Vec<(Pubkey, u64)>> {
    let entries = list_lst_info_records(rpc, pool_id)
        .await?
        .into_iter()
        .map(|record| {
            let mint = record
                .mint
                .ok_or_else(|| anyhow!("could not read mint from LstInfo {}", record.address))?;
            Ok((record.address, mint))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut balances = Vec::with_capacity(entries.len());

    for chunk in entries.chunks(100) {
        let token_accounts = chunk
            .iter()
            .map(|(_, mint)| token_account_address(pool_id, mint))
            .collect_vec();
        let accounts = rpc.get_multiple_accounts(&token_accounts).await?;

        for ((_, mint), account) in chunk.iter().zip(accounts) {
            let amount = account
                .as_ref()
                .and_then(token_amount_from_account)
                .unwrap_or(0);
            if amount > 0 {
                balances.push((*mint, amount));
            }
        }
    }

    balances.sort_by_key(|(mint, _)| *mint);
    Ok(balances)
}

struct LstInfoRecord {
    address: Pubkey,
    mint: Option<Pubkey>,
    version: &'static str,
    is_active: Option<bool>,
    v3: Option<LstInfoAccount>,
}

async fn list_lst_info_records(rpc: &RpcClient, pool_id: &Pubkey) -> Result<Vec<LstInfoRecord>> {
    let accounts = rpc
        .get_program_accounts_with_config(
            &ID_CONST,
            RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..RpcAccountInfoConfig::default()
                },
                filters: Some(vec![
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        0,
                        LstInfoAccount::DISCRIMINATOR,
                    )),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(8, &pool_id.to_bytes())),
                ]),
                ..RpcProgramAccountsConfig::default()
            },
        )
        .await?;

    let mut entries = Vec::new();
    for (address, account) in accounts {
        let version = lst_info_version(account.data.len());
        let mut v3 = None;
        if is_v3_lst_info_data_len(account.data.len()) {
            let mut data = account.data.as_slice();
            v3 = Some(LstInfoAccount::try_deserialize(&mut data)?);
        }

        let (mint, is_active) = if let Some(lst_info) = v3.as_ref() {
            (Some(lst_info.mint), Some(lst_info.is_active))
        } else {
            (
                pubkey_from_account_data(&account.data, LST_INFO_MINT_OFFSET),
                bool_from_account_data(&account.data, LST_INFO_IS_ACTIVE_OFFSET),
            )
        };

        entries.push(LstInfoRecord {
            address,
            mint,
            version,
            is_active,
            v3,
        });
    }
    entries.sort_by_key(|entry| (entry.mint.unwrap_or_default(), entry.address));
    Ok(entries)
}

fn print_lst_info_records(records: &[LstInfoRecord]) {
    println!("Found {} LstInfo PDA accounts:", records.len());
    for record in records {
        println!(
            "  mint={} lst_info={} version={} is_active={}",
            optional_pubkey_display(record.mint),
            record.address,
            record.version,
            optional_bool_display(record.is_active)
        );
    }
}

fn lst_info_version(data_len: usize) -> &'static str {
    if is_v3_lst_info_data_len(data_len) {
        "v3"
    } else {
        "v2"
    }
}

fn is_v3_lst_info_data_len(data_len: usize) -> bool {
    data_len >= LST_INFO_V3_DATA_LEN
}

fn pubkey_from_account_data(data: &[u8], offset: usize) -> Option<Pubkey> {
    let bytes: [u8; PUBKEY_DATA_LEN] = data
        .get(offset..offset + PUBKEY_DATA_LEN)?
        .try_into()
        .ok()?;
    Some(Pubkey::new_from_array(bytes))
}

fn bool_from_account_data(data: &[u8], offset: usize) -> Option<bool> {
    Some(*data.get(offset)? != 0)
}

fn optional_pubkey_display(value: Option<Pubkey>) -> String {
    value
        .map(|pubkey| pubkey.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn optional_bool_display(value: Option<bool>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn get_unstake_accounts(
    stake_pool_program: &Pubkey,
    stake_pool_address: &Pubkey,
    stake_pool_state: &spl_stake_pool::state::StakePool,
    stake_pool_validator_list: &spl_stake_pool::state::ValidatorList,
    amount_in: u64,
) -> Result<UnstakeAccountSelection> {
    #[derive(Clone)]
    struct AccountInfo {
        is_preferred: bool,
        stake_address: Pubkey,
        lamports: u64,
    }

    let mut lst_amounts = Vec::new();

    let accounts = stake_pool_validator_list
        .validators
        .iter()
        .filter(|validator_info| validator_info.status == StakeStatus::Active.into())
        .filter(|validator_info| Into::<u64>::into(validator_info.active_stake_lamports) != 0u64)
        .map(|validator_info| {
            let stake_account_address = find_stake_program_address(
                stake_pool_program,
                &validator_info.vote_account_address,
                stake_pool_address,
                None,
            )
            .0;

            let is_preferred = stake_pool_state.preferred_withdraw_validator_vote_address
                == Some(validator_info.vote_account_address);

            let active_stake_lamports: u64 =
                Into::<u64>::into(validator_info.active_stake_lamports);

            AccountInfo {
                is_preferred,
                stake_address: stake_account_address,
                lamports: active_stake_lamports,
            }
        })
        .collect::<Vec<_>>();

    let mut remaining_amount = amount_in;

    let fee = &stake_pool_state.stake_withdrawal_fee;
    let inverse_fee_numerator = fee.denominator - fee.numerator;
    let inverse_fee_denominator = fee.denominator;

    let calc_pool_tokens_for_deposit = |stake_lamports: u64| -> u128 {
        if stake_pool_state.pool_token_supply == 0 || stake_pool_state.total_lamports == 0 {
            return stake_lamports as u128;
        }
        let numerator = stake_lamports as u128 * stake_pool_state.pool_token_supply as u128;
        numerator / stake_pool_state.total_lamports as u128
    };

    let mut withdraw_from = Vec::<AccountInfo>::new();

    for is_preferred in [true, false].iter() {
        let filtered_accounts = accounts
            .iter()
            .filter(|a| a.is_preferred == *is_preferred)
            .sorted_by(|a, b| b.lamports.cmp(&a.lamports));

        for account in filtered_accounts {
            let mut available_for_withdrawal = calc_pool_tokens_for_deposit(account.lamports);

            if inverse_fee_numerator != 0 {
                available_for_withdrawal = available_for_withdrawal
                    .mul(inverse_fee_denominator as u128)
                    .div(inverse_fee_numerator as u128);
            }

            let pool_amount = (available_for_withdrawal as u64).min(remaining_amount);

            if pool_amount == 0 {
                continue;
            }

            withdraw_from.push(account.clone());
            lst_amounts.push(pool_amount);

            remaining_amount -= pool_amount;

            if remaining_amount == 0 {
                break;
            }
        }

        if remaining_amount == 0 {
            break;
        }
    }

    if remaining_amount > 0 {
        return Err(anyhow!("not enough pool tokens to unstake"));
    }

    withdraw_from.iter().for_each(|account| {
        println!(
            "Withdrawing from stake account {} that has {} lamports",
            account.stake_address, account.lamports
        );
    });

    let withdraw_stake_accounts = withdraw_from
        .iter()
        .map(|address| address.stake_address)
        .collect_vec();
    let new_stake_accounts = withdraw_from
        .iter()
        .map(|_| Keypair::new())
        .collect::<Vec<_>>();
    let new_stake_pda_accounts = new_stake_accounts
        .iter()
        .map(|stake_account_keypair| {
            Pubkey::find_program_address(
                &[
                    b"stake_account_info",
                    &stake_account_keypair.pubkey().to_bytes(),
                ],
                &ID_CONST,
            )
            .0
        })
        .collect::<Vec<_>>();

    Ok((
        lst_amounts,
        withdraw_stake_accounts,
        new_stake_accounts
            .into_iter()
            .map(PubkeyOrKeypair::Keypair)
            .collect(),
        new_stake_pda_accounts,
    ))
}

fn get_unstake_accounts_with_new_stake_account_as_pda(
    stake_pool_program: &Pubkey,
    stake_pool_address: &Pubkey,
    stake_pool_state: &spl_stake_pool::state::StakePool,
    stake_pool_validator_list: &spl_stake_pool::state::ValidatorList,
    stake_account_seed: u64,
    payer: &Pubkey,
    amount_in: u64,
) -> Result<UnstakeAccountSelection> {
    #[derive(Clone)]
    struct AccountInfo {
        is_preferred: bool,
        stake_address: Pubkey,
        lamports: u64,
    }

    let mut lst_amounts = Vec::new();

    let accounts = stake_pool_validator_list
        .validators
        .iter()
        .filter(|validator_info| validator_info.status == StakeStatus::Active.into())
        .filter(|validator_info| Into::<u64>::into(validator_info.active_stake_lamports) != 0u64)
        .map(|validator_info| {
            let stake_account_address = find_stake_program_address(
                stake_pool_program,
                &validator_info.vote_account_address,
                stake_pool_address,
                None,
            )
            .0;

            let is_preferred = stake_pool_state.preferred_withdraw_validator_vote_address
                == Some(validator_info.vote_account_address);

            let active_stake_lamports: u64 =
                Into::<u64>::into(validator_info.active_stake_lamports);

            AccountInfo {
                is_preferred,
                stake_address: stake_account_address,
                lamports: active_stake_lamports,
            }
        })
        .collect::<Vec<_>>();

    let mut remaining_amount = amount_in;

    let fee = &stake_pool_state.stake_withdrawal_fee;
    let inverse_fee_numerator = fee.denominator - fee.numerator;
    let inverse_fee_denominator = fee.denominator;

    let calc_pool_tokens_for_deposit = |stake_lamports: u64| -> u128 {
        if stake_pool_state.pool_token_supply == 0 || stake_pool_state.total_lamports == 0 {
            return stake_lamports as u128;
        }
        let numerator = stake_lamports as u128 * stake_pool_state.pool_token_supply as u128;
        numerator / stake_pool_state.total_lamports as u128
    };

    let mut withdraw_from = Vec::<AccountInfo>::new();

    for is_preferred in [true, false].iter() {
        let filtered_accounts = accounts
            .iter()
            .filter(|a| a.is_preferred == *is_preferred)
            .sorted_by(|a, b| b.lamports.cmp(&a.lamports));

        for account in filtered_accounts {
            let mut available_for_withdrawal = calc_pool_tokens_for_deposit(account.lamports);

            if inverse_fee_numerator != 0 {
                available_for_withdrawal = available_for_withdrawal
                    .mul(inverse_fee_denominator as u128)
                    .div(inverse_fee_numerator as u128);
            }

            let pool_amount = (available_for_withdrawal as u64).min(remaining_amount);

            if pool_amount == 0 {
                continue;
            }

            withdraw_from.push(account.clone());
            lst_amounts.push(pool_amount);

            remaining_amount -= pool_amount;

            if remaining_amount == 0 {
                break;
            }
        }

        if remaining_amount == 0 {
            break;
        }
    }

    if remaining_amount > 0 {
        return Err(anyhow!("not enough pool tokens to unstake"));
    }

    withdraw_from.iter().for_each(|account| {
        println!(
            "Withdrawing from stake account {} that has {} lamports",
            account.stake_address, account.lamports
        );
    });

    let withdraw_stake_accounts = withdraw_from
        .iter()
        .map(|address| address.stake_address)
        .collect_vec();
    let new_stake_accounts = withdraw_from
        .iter()
        .enumerate()
        .map(|(i, _)| {
            Pubkey::find_program_address(
                &[
                    b"stake_account",
                    payer.as_ref(),
                    (stake_account_seed + i as u64).to_le_bytes().as_ref(),
                ],
                &ID_CONST,
            )
            .0
        })
        .collect::<Vec<_>>();
    let new_stake_pda_accounts = new_stake_accounts
        .iter()
        .map(|stake_account| {
            Pubkey::find_program_address(
                &[b"stake_account_info", stake_account.as_ref()],
                &ID_CONST,
            )
            .0
        })
        .collect::<Vec<_>>();

    Ok((
        lst_amounts,
        withdraw_stake_accounts,
        new_stake_accounts
            .into_iter()
            .map(PubkeyOrKeypair::Pubkey)
            .collect(),
        new_stake_pda_accounts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_lst_amounts_pads_to_protocol_width() {
        assert_eq!(pad_lst_amounts(vec![7, 11]).unwrap(), [7, 11, 0, 0, 0]);
    }

    #[test]
    fn pad_lst_amounts_rejects_empty_selection() {
        assert!(pad_lst_amounts(vec![]).is_err());
    }

    #[test]
    fn pad_lst_amounts_rejects_more_than_five_legs() {
        assert!(pad_lst_amounts(vec![1, 2, 3, 4, 5, 6]).is_err());
    }

    #[test]
    fn lst_info_v3_size_filter_skips_pre_v3_accounts() {
        assert!(!is_v3_lst_info_data_len(LST_INFO_V3_DATA_LEN - 1));
        assert!(is_v3_lst_info_data_len(LST_INFO_V3_DATA_LEN));
        assert!(is_v3_lst_info_data_len(LST_INFO_V3_DATA_LEN + 16));
    }

    #[test]
    fn lst_info_raw_prefix_reads_v2_fields() {
        let mint = Pubkey::new_unique();
        let mut data = vec![0_u8; LST_INFO_IS_ACTIVE_OFFSET + 1];
        data[LST_INFO_MINT_OFFSET..LST_INFO_MINT_OFFSET + PUBKEY_DATA_LEN]
            .copy_from_slice(mint.as_ref());
        data[LST_INFO_IS_ACTIVE_OFFSET] = 1;

        assert_eq!(
            pubkey_from_account_data(&data, LST_INFO_MINT_OFFSET),
            Some(mint)
        );
        assert_eq!(
            bool_from_account_data(&data, LST_INFO_IS_ACTIVE_OFFSET),
            Some(true)
        );
        assert_eq!(lst_info_version(data.len()), "v2");
    }

    #[test]
    fn pending_inventory_entries_keeps_all_entries_when_not_resuming() {
        let current_session_id = 42;
        let entries = vec![
            test_inventory_entry(current_session_id),
            test_inventory_entry(current_session_id - 1),
        ];

        let (pending_entries, resume_info) =
            pending_inventory_entries_for_sync(entries, false, current_session_id);

        assert_eq!(pending_entries.len(), 2);
        assert_eq!(resume_info, None);
    }

    #[test]
    fn pending_inventory_entries_skips_entries_already_synced_in_current_session() {
        let current_session_id = 42;
        let old_entry = test_inventory_entry(current_session_id - 1);
        let never_synced_entry = test_inventory_entry(0);
        let already_synced_entry = test_inventory_entry(current_session_id);
        let expected_pending_mints = vec![old_entry.mint, never_synced_entry.mint];
        let entries = vec![already_synced_entry, old_entry, never_synced_entry];

        let (pending_entries, resume_info) =
            pending_inventory_entries_for_sync(entries, true, current_session_id);

        assert_eq!(
            pending_entries.iter().map(|entry| entry.mint).collect_vec(),
            expected_pending_mints
        );
        assert_eq!(
            resume_info,
            Some(InventorySyncResumeInfo {
                session_id: current_session_id,
                already_synced: 1,
                total: 3,
            })
        );
    }

    fn test_inventory_entry(last_synced_session_id: u32) -> InventoryEntry {
        InventoryEntry {
            mint: Pubkey::new_unique(),
            pool_lst_token_account: Pubkey::new_unique(),
            stake_pool: Pubkey::new_unique(),
            lst_info: Pubkey::new_unique(),
            last_synced_session_id,
            rate_history_epochs: [0; 5],
            rate_history_rates: [0; 5],
            rate_history_len: 0,
        }
    }

    #[test]
    fn compare_entry_filter_allows_disabled_only_when_explicitly_selected() {
        let enabled_mint = Pubkey::new_unique();
        let disabled_mint = Pubkey::new_unique();
        let enabled_entry = test_lst_info(enabled_mint, true);
        let disabled_entry = test_lst_info(disabled_mint, false);

        assert!(include_compare_lst_entry(&enabled_entry, None, false));
        assert!(!include_compare_lst_entry(&disabled_entry, None, false));
        assert!(!include_compare_lst_entry(&disabled_entry, None, true));

        let selected_disabled = HashSet::from([disabled_mint]);
        assert!(!include_compare_lst_entry(
            &disabled_entry,
            Some(&selected_disabled),
            false,
        ));
        assert!(include_compare_lst_entry(
            &disabled_entry,
            Some(&selected_disabled),
            true,
        ));
        assert!(!include_compare_lst_entry(
            &enabled_entry,
            Some(&selected_disabled),
            true,
        ));
    }

    fn test_lst_info(mint: Pubkey, enabled: bool) -> LstInfoAccount {
        LstInfoAccount {
            pool: Pubkey::new_unique(),
            mint,
            stake_pool: Pubkey::new_unique(),
            stake_pool_program: Pubkey::new_unique(),
            bump: 0,
            is_active: false,
            last_synced_session_id: 0,
            enabled,
            reserved: [0],
            rate_history_epochs: [0; 5],
            rate_history_rates: [0; 5],
            rate_history_len: 0,
        }
    }

    #[test]
    fn load_wallet_accepts_pubkey_when_dumping() {
        let pubkey = Pubkey::new_unique();
        let value = pubkey.to_string();

        match load_wallet(Some(&value), true).unwrap() {
            Wallet::Pubkey(loaded) => assert_eq!(loaded, pubkey),
            Wallet::Keypair(_) => panic!("expected pubkey wallet"),
        }
    }

    #[test]
    fn load_wallet_rejects_pubkey_without_dumping() {
        let value = Pubkey::new_unique().to_string();

        assert!(load_wallet(Some(&value), false).is_err());
    }

    #[test]
    fn parse_sol_lamports_accepts_decimal_notional() {
        assert_eq!(parse_sol_lamports("0.1").unwrap(), 100_000_000);
        assert_eq!(parse_sol_lamports("1").unwrap(), LAMPORTS_PER_SOL);
        assert_eq!(parse_sol_lamports(".000000001").unwrap(), 1);
    }

    #[test]
    fn parse_sol_lamports_rejects_over_precise_notional() {
        assert!(parse_sol_lamports("0.0000000001").is_err());
    }

    #[test]
    fn parse_percent_units_accepts_plain_and_suffixed_percentages() {
        assert_eq!(parse_percent_units("10", "cap").unwrap(), 100_000);
        assert_eq!(parse_percent_units("10%", "cap").unwrap(), 100_000);
        assert_eq!(parse_percent_units("0.125%", "cap").unwrap(), 1_250);
        assert_eq!(format_percent_units(1_250), "0.125%");
    }

    #[test]
    fn parse_percent_units_rejects_invalid_percentages() {
        assert!(parse_percent_units("100.0001", "cap").is_err());
        assert!(parse_percent_units("0.00001", "cap").is_err());
        assert!(parse_percent_units("-1", "cap").is_err());
    }

    #[test]
    fn balanced_plan_reduces_lsts_by_current_value_ratio() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(50),
            0,
            0,
            0,
            parse_percent_units("10", "cap").unwrap(),
            0,
            vec![
                test_pool_lst_position(mint_a, sol_lamports(30)),
                test_pool_lst_position(mint_b, sol_lamports(20)),
            ],
            &[],
        )
        .unwrap();
        let positions = plan_positions_by_mint(&plan);

        assert_eq!(
            plan.current_lst_value_lamports,
            u128::from(sol_lamports(50))
        );
        assert_eq!(plan.tvl_lamports, u128::from(sol_lamports(100)));
        assert_eq!(plan.target_lst_value_lamports, u128::from(sol_lamports(10)));
        assert_eq!(plan.new_lst_value_lamports, u128::from(sol_lamports(10)));
        assert_eq!(positions[&mint_a].target_amount, sol_lamports(6));
        assert_eq!(positions[&mint_a].unstake_amount, sol_lamports(24));
        assert_eq!(positions[&mint_b].target_amount, sol_lamports(4));
        assert_eq!(positions[&mint_b].unstake_amount, sol_lamports(16));
    }

    #[test]
    fn balanced_plan_applies_overrides_and_preserves_remaining_ratio() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let mint_c = Pubkey::new_unique();
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(30),
            0,
            0,
            0,
            parse_percent_units("10", "cap").unwrap(),
            0,
            vec![
                test_pool_lst_position(mint_a, sol_lamports(30)),
                test_pool_lst_position(mint_b, sol_lamports(20)),
                test_pool_lst_position(mint_c, sol_lamports(20)),
            ],
            &[PoolLstTargetOverride {
                mint: mint_a,
                percent: parse_percent_units("2", "override").unwrap(),
            }],
        )
        .unwrap();
        let positions = plan_positions_by_mint(&plan);

        assert_eq!(plan.tvl_lamports, u128::from(sol_lamports(100)));
        assert_eq!(plan.new_lst_value_lamports, u128::from(sol_lamports(10)));
        assert_eq!(positions[&mint_a].target_amount, sol_lamports(2));
        assert_eq!(positions[&mint_b].target_amount, sol_lamports(4));
        assert_eq!(positions[&mint_c].target_amount, sol_lamports(4));
    }

    #[test]
    fn balanced_plan_rejects_overrides_above_global_cap() {
        let mint_a = Pubkey::new_unique();
        let err = build_balanced_pool_lst_plan(
            sol_lamports(40),
            0,
            0,
            0,
            parse_percent_units("10", "cap").unwrap(),
            0,
            vec![test_pool_lst_position(mint_a, sol_lamports(60))],
            &[PoolLstTargetOverride {
                mint: mint_a,
                percent: parse_percent_units("20", "override").unwrap(),
            }],
        )
        .unwrap_err();

        assert!(err.to_string().contains("above global LST cap"));
    }

    #[test]
    fn balanced_plan_does_not_reduce_until_trigger_buffer_is_exceeded() {
        let mint = Pubkey::new_unique();
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(94) + 950_000_000,
            0,
            0,
            0,
            parse_percent_units("5", "cap").unwrap(),
            0,
            vec![test_pool_lst_position(mint, sol_lamports(5) + 50_000_000)],
            &[],
        )
        .unwrap();
        let positions = plan_positions_by_mint(&plan);

        assert_eq!(
            plan.trigger_percent,
            parse_percent_units("5.1", "cap").unwrap()
        );
        assert_eq!(positions[&mint].target_amount, sol_lamports(5) + 50_000_000);
        assert_eq!(positions[&mint].unstake_amount, 0);
    }

    #[test]
    fn balanced_plan_skip_message_reports_trigger_buffer_not_exceeded() {
        let mint = Pubkey::new_unique();
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(94) + 950_000_000,
            0,
            0,
            0,
            parse_percent_units("5", "cap").unwrap(),
            0,
            vec![test_pool_lst_position(mint, sol_lamports(5) + 50_000_000)],
            &[],
        )
        .unwrap();

        assert_eq!(
            balanced_pool_lst_skip_message(&plan).as_deref(),
            Some("Current LST value 5.05% is less than trigger 5.1%; skipping unstakes")
        );
    }

    #[test]
    fn balanced_plan_does_not_mark_dust_when_trigger_buffer_is_not_exceeded() {
        let mint = Pubkey::new_unique();
        let half_sol = LAMPORTS_PER_SOL / 2;
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(99) + half_sol,
            0,
            0,
            0,
            parse_percent_units("1", "cap").unwrap(),
            LAMPORTS_PER_SOL,
            vec![test_pool_lst_position(mint, half_sol)],
            &[],
        )
        .unwrap();
        let positions = plan_positions_by_mint(&plan);

        assert_eq!(positions[&mint].target_amount, half_sol);
        assert_eq!(positions[&mint].unstake_amount, 0);
        assert_eq!(positions[&mint].note, None);
    }

    #[test]
    fn balanced_plan_does_not_apply_override_until_trigger_buffer_is_exceeded() {
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(70),
            0,
            0,
            0,
            parse_percent_units("40", "cap").unwrap(),
            0,
            vec![
                test_pool_lst_position(mint_a, sol_lamports(20)),
                test_pool_lst_position(mint_b, sol_lamports(10)),
            ],
            &[PoolLstTargetOverride {
                mint: mint_a,
                percent: parse_percent_units("5", "override").unwrap(),
            }],
        )
        .unwrap();
        let positions = plan_positions_by_mint(&plan);

        assert_eq!(plan.tvl_lamports, u128::from(sol_lamports(100)));
        assert_eq!(plan.new_lst_value_lamports, u128::from(sol_lamports(30)));
        assert_eq!(positions[&mint_a].target_amount, sol_lamports(20));
        assert_eq!(positions[&mint_a].unstake_amount, 0);
        assert_eq!(positions[&mint_b].target_amount, sol_lamports(10));
        assert_eq!(positions[&mint_b].unstake_amount, 0);
    }

    #[test]
    fn balanced_plan_unstakes_all_when_target_is_less_than_one_sol() {
        let mint = Pubkey::new_unique();
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(98),
            0,
            0,
            0,
            parse_percent_units("0.5", "cap").unwrap(),
            0,
            vec![test_pool_lst_position(mint, sol_lamports(2))],
            &[],
        )
        .unwrap();
        let positions = plan_positions_by_mint(&plan);

        assert_eq!(positions[&mint].target_amount, 0);
        assert_eq!(positions[&mint].unstake_amount, sol_lamports(2));
        assert_eq!(
            positions[&mint].note.as_deref(),
            Some("target below 1 SOL; planning full LST unstake")
        );
    }

    #[test]
    fn balanced_plan_skips_unstake_when_full_balance_is_below_minimum_split() {
        let mint = Pubkey::new_unique();
        let half_sol = LAMPORTS_PER_SOL / 2;
        let plan = build_balanced_pool_lst_plan(
            sol_lamports(99),
            0,
            0,
            0,
            parse_percent_units("0", "cap").unwrap(),
            LAMPORTS_PER_SOL,
            vec![test_pool_lst_position(mint, half_sol)],
            &[],
        )
        .unwrap();
        let positions = plan_positions_by_mint(&plan);

        assert_eq!(positions[&mint].target_amount, half_sol);
        assert_eq!(positions[&mint].unstake_amount, 0);
        assert!(positions[&mint]
            .note
            .as_deref()
            .unwrap()
            .contains("below minimum"));
    }

    #[test]
    fn advantage_bps_is_positive_when_v3_is_better() {
        assert_eq!(advantage_bps(1_000, 100_000), Some(100.0));
        assert_eq!(advantage_bps(-500, 100_000), Some(-50.0));
        assert_eq!(advantage_bps(1, 0), None);
    }

    #[test]
    fn csv_render_can_omit_header_for_polling() {
        let record = CompareRecord {
            timestamp_unix: 1,
            pool: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            direction: CompareDirection::SolToLst,
            notional_sol_lamports: LAMPORTS_PER_SOL,
            lst_amount: Some(2),
            jupiter_sol_lamports: Some(3),
            jupiter_lst_amount: Some(2),
            v3_sol_lamports: Some(1),
            v3_lst_amount: Some(2),
            v3_advantage_lamports: Some(2),
            v3_advantage_bps: Some(3.0),
            jupiter_route: vec!["Route".to_string()],
            error: None,
        };

        let with_header = render_csv_compare_records(std::slice::from_ref(&record), true);
        assert!(with_header.starts_with(csv_compare_header()));

        let without_header = render_csv_compare_records(&[record], false);
        assert!(!without_header.starts_with(csv_compare_header()));
        assert!(without_header.ends_with(",true\n"));
    }

    fn test_pool_lst_position(mint: Pubkey, value: u64) -> PoolLstPositionSnapshot {
        PoolLstPositionSnapshot {
            mint,
            amount: value,
            sol_value: value,
            stake_pool_program_id: spl_stake_pool::id(),
            stake_pool_total_lamports: 1,
            stake_pool_token_supply: 1,
            stake_withdrawal_fee: spl_stake_pool::state::Fee::default(),
        }
    }

    fn sol_lamports(sol: u64) -> u64 {
        sol.checked_mul(LAMPORTS_PER_SOL).unwrap()
    }

    fn plan_positions_by_mint(
        plan: &BalancedPoolLstPlan,
    ) -> HashMap<Pubkey, &BalancedPoolLstPlanPosition> {
        plan.positions
            .iter()
            .map(|position| (position.mint, position))
            .collect()
    }

    #[test]
    fn calculate_lamports_to_withdraw_for_one_vlp_matches_share_price() {
        assert_eq!(
            calculate_lamports_to_withdraw_for_lp(
                1_000_000_000,
                LAMPORTS_PER_SOL,
                2_000_000_000,
                0,
                0
            )
            .unwrap(),
            2_000_000_000
        );
    }

    #[test]
    fn calculate_lamports_to_withdraw_applies_lp_fee_units() {
        assert_eq!(
            calculate_lamports_to_withdraw_for_lp(
                1_000_000_000,
                LAMPORTS_PER_SOL,
                1_000_000_000,
                50,
                0
            )
            .unwrap(),
            999_500_000
        );
    }

    #[test]
    fn calculate_tokens_to_mint_uses_initial_supply_branch() {
        assert_eq!(
            calculate_tokens_to_mint_for_deposit(0, LAMPORTS_PER_SOL, 500, 0).unwrap(),
            LAMPORTS_PER_SOL + 500
        );
    }

    #[test]
    fn calculate_tokens_to_mint_subtracts_unvested_rewards_in_share_branch() {
        assert_eq!(
            calculate_tokens_to_mint_for_deposit(1_000, 100, 2_000, 500).unwrap(),
            66
        );
    }

    #[test]
    fn epoch_progress_from_slot_index_uses_full_u64_range() {
        assert_eq!(epoch_progress_from_slot_index(0, 10).unwrap(), 0);
        assert_eq!(
            epoch_progress_from_slot_index(5, 10).unwrap(),
            (u64::MAX as u128 * 5 / 10) as u64
        );
        assert!(epoch_progress_from_slot_index(1, 0).is_err());
    }

    #[test]
    fn format_lamports_as_sol_keeps_nine_decimals() {
        assert_eq!(format_lamports_as_sol(1_234_567_890), "1.234567890");
    }
}
