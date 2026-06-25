#![allow(clippy::too_many_arguments)]

use std::{
    collections::HashSet,
    mem::{offset_of, size_of},
    ops::{Div, Mul},
    rc::Rc,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
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

        if entries.is_empty() {
            println!("No active v3 LST inventory entries found for sync");
            vec![vec![]]
        } else {
            println!("Syncing {} active v3 LST inventory entries:", entries.len());
            for entry in &entries {
                println!(
                    "  mint={} stake_pool={} pool_lst_token_account={} lst_info={}",
                    entry.mint, entry.stake_pool, entry.pool_lst_token_account, entry.lst_info
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
    rate_history_epochs: [u64; 5],
    rate_history_rates: [u64; 5],
    rate_history_len: u8,
}

enum InventoryValueCheck {
    CurrentValue(u64),
    NeedsSync(String),
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
    .await?;
    if requests.is_empty() {
        println!("No pool-owned LST balances found");
        return Ok(());
    }

    let mut next_stake_account_seed = stake_account_seed;
    for (mint, amount) in requests {
        let pool = fetch_pool(program, *pool_id).await?;
        let seed = next_stake_account_seed.unwrap_or(pool.total_deactivating_stake);
        let (stake_pool_program_id, _) =
            get_stake_pool_for_mint_from_supported_programs(&program.rpc(), &mint).await?;

        println!("Unstaking {amount} tokens for mint {mint}");
        let new_stake_account_count = unstake_pool_lsts(
            program,
            pool_id,
            wallet,
            &stake_pool_program_id,
            &mint,
            &pool,
            amount,
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
        total_sol_value_without_discount,
        half_stake_pool_fee_pct,
        lst_cost,
        dynamic_fee_pct,
        pool_fee,
        manager_fee,
        total_fee,
        total_cost,
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
    let current_rate = (stake_pool.total_lamports as u128)
        .checked_mul(RATE_SCALE)
        .ok_or_else(|| anyhow!("rate overflow"))?
        .checked_div(stake_pool.pool_token_supply as u128)
        .ok_or_else(|| anyhow!("rate underflow"))? as u64;

    if rate_drift_exceeds(&lst_info, current_rate, pool.max_rate_drift_bps) {
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
