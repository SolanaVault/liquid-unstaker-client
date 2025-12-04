use std::{
    mem::offset_of, ops::{Div, Mul}, rc::Rc, str::FromStr, u64
};

use anchor_client::{
    solana_client::{
        nonblocking::rpc_client::RpcClient,
        rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig},
        rpc_filter::{Memcmp, RpcFilterType},
    },
    solana_sdk::{
        self, pubkey::Pubkey, signature::{read_keypair_file, Keypair}, signer::Signer, system_instruction::create_account, transaction::Transaction
    },
    Client,
};
use anchor_lang::prelude::*;
use anchor_spl::associated_token;
use anchor_spl::{
    token::spl_token, token_interface::spl_token_metadata_interface::borsh::BorshDeserialize,
};
use anyhow::Result;
use clap::{Arg, Command};
use fee::{Fee, FEE_PCT_DIVISOR};
use itertools::{izip, Itertools};
use solana_account_decoder::UiAccountEncoding;
use spl_stake_pool::{
    find_stake_program_address,
    state::{StakePool, StakeStatus},
};

mod fee;
mod error;

const SANCTUM_SINGLE_VALIDATOR_STAKE_POOL_PROGRAM: Pubkey =
    pubkey!("SP12tWFxD9oJsVWNavTTBZvMbA6gkAmxtVgxdqvyvhY");
const SANCTUM_MULTIPLE_VALIDATORS_STAKE_POOL_PROGRAM: Pubkey =
    pubkey!("SPMBzsVUuoHA4Jm6KunbsotaahvVikZs1JyTW6iJvbn");

const SUPPORTED_STAKE_POOL_PROGRAMS: [Pubkey; 3] = [
    spl_stake_pool::id(),
    SANCTUM_SINGLE_VALIDATOR_STAKE_POOL_PROGRAM,
    SANCTUM_MULTIPLE_VALIDATORS_STAKE_POOL_PROGRAM,
];

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

#[tokio::main]
async fn main() -> Result<()> {
    // Define the CLI using clap
    let matches = Command::new("Liquid Unstaker Client")
        .version("0.1")
        .arg(Arg::new("pool")
            .long("pool")
            .help("The liquid unstake pool ID")
            .required(true)
        )
        .arg(
            Arg::new("simulate")
                .long("simulate")
                .help("Simulate the transaction without sending it")
                .value_parser(clap::value_parser!(bool))
                .action(clap::ArgAction::SetTrue)
                .required(false)
        )
        .arg(
            Arg::new("no-stake-account-as-pda")
                .long("no-stake-account-as-pda")
                .help("Do not create new stake accounts as PDAs (for unstake-lst and unstake-lst-wrapped commands)")
                .value_parser(clap::value_parser!(bool))
                .action(clap::ArgAction::SetTrue)
                .required(false)
        )
        .arg(
            Arg::new("rpc")
                .long("rpc")
                .help("The URL of the Solana RPC")
                .required(true)
        )
        .arg(
            Arg::new("keypair")
                .long("keypair")
                .help("Wallet for any transactions sent, e.g. unstake lst/stake account, withdraw, deposit")
        )
        .subcommand(
            Command::new("deposit")
                .about("Deposit into the liquid unstake pool and receive an LP token back")
                .arg(
                    Arg::new("lamports")
                        .help("Amount to deposit")
                        .required(true)
                        .value_parser(clap::value_parser!(u64))
                ),
        )
        .subcommand(
            Command::new("withdraw")
                .about("Withdraw from the liquid unstake pool and receive SOL back")
                .arg(
                    Arg::new("tokens")
                        .help("Amount of LP tokens to deposit in order to withdraw corresponding lamports from the pool")
                        .required(true)
                        .value_parser(clap::value_parser!(u64))
                ),
        )
        .subcommand(
            Command::new("unstake-lst")
                .about("Unstake the LST from the pool and receive SOL back")
                .arg(
                    Arg::new("mint")
                        .help("Mint of the LST token")
                        .required(true)
                )
                .arg(
                    Arg::new("amount")
                        .help("Amount of LP tokens to deposit in order to withdraw corresponding lamports from the pool")
                        .required(true)
                        .value_parser(clap::value_parser!(u64))
                ),
        )
        .subcommand(
            Command::new("unstake-lst-wrapped")
                .about("Unstake the LST from the pool and receive WSOL back (V2 only)")
                .arg(
                    Arg::new("mint")
                        .help("Mint of the LST token")
                        .required(true)
                )
                .arg(
                    Arg::new("amount")
                        .help("Amount of LP tokens to deposit in order to withdraw corresponding lamports from the pool")
                        .required(true)
                        .value_parser(clap::value_parser!(u64))
                ),
        )
        .subcommand(
            Command::new("quote-unstake-lst")
                .about("Get a quote of how many lamports would be received by unstaking the given amount of LST tokens (V2 only)")
                .arg(
                    Arg::new("mint")
                        .help("Mint of the LST token")
                        .required(true)
                )
                .arg(
                    Arg::new("amount")
                        .help("Amount of LP tokens to deposit in order to withdraw corresponding lamports from the pool")
                        .required(true)
                        .value_parser(clap::value_parser!(u64))
                ),
        )
        .subcommand(
            Command::new("quote-unstake-lst-wrapped")
                .about("Get a quote of how many lamports would be received by unstaking the given amount of LST tokens")
                .arg(
                    Arg::new("mint")
                        .help("Mint of the LST token")
                        .required(true)
                )
                .arg(
                    Arg::new("amount")
                        .help("Amount of LP tokens to deposit in order to withdraw corresponding lamports from the pool")
                        .required(true)
                        .value_parser(clap::value_parser!(u64))
                ),
        )
        .subcommand(
            Command::new("list-lst-mints")
                .about("List all LST mints supported by the unstake pool")
                .arg(
                    Arg::new("limit")
                        .help("Maximum number of mints to return")
                        .required(false)
                        .value_parser(clap::value_parser!(u64))
                )
        )
        .subcommand(
            Command::new("pool-info")
                .about("Get information about the unstake pool")
        )
        .get_matches();

    // Extract arguments
    let rpc_url: &String = matches.get_one("rpc").unwrap();
    let unstake_pool_id = Pubkey::from_str(matches.get_one::<String>("pool").unwrap()).unwrap();
    let simulate = *matches.get_one::<bool>("simulate").unwrap_or(&false);
    let new_stake_account_as_pda = !*matches.get_one::<bool>("no-stake-account-as-pda").unwrap_or(&false);
    let wallet_keypair = {
        
        // Load the wallet keypair file
        let wallet_keypair_path: Option<&String> = matches.get_one("keypair");

        if let Some(wallet_keypair_path) = wallet_keypair_path {
            read_keypair_file(wallet_keypair_path)
                .map_err(|_| anyhow::anyhow!("Failed to read wallet keypair file {}", wallet_keypair_path))
        } else {
            Ok(Keypair::new())
        }
    }?;

    // Set up the anchor client
    let client = Client::new(
        anchor_client::Cluster::Custom(rpc_url.clone(), rpc_url.clone()),
        Rc::new(wallet_keypair.insecure_clone()),
    );
    let program: anchor_client::Program<Rc<Keypair>> =
        client.program(liquid_unstaker::liquid_unstaker::ID_CONST)?;

    // Load unstake pool info
    let unstake_pool_info = program
        .account::<liquid_unstaker::liquid_unstaker::accounts::Pool>(unstake_pool_id)
        .await?;

    // Add logic for each command here
    match matches.subcommand() {
        Some(("quote-unstake-lst", arg_matches)) => {
            let rpc = program.rpc();
            let mint: Pubkey =
                Pubkey::from_str(arg_matches.get_one::<String>("mint").unwrap()).unwrap();

            // Make sure to get the right stake pool program id for this mint
            let spl_stake_pool_program_id =
                get_stake_pool_program_for_lst_mint(&rpc, &mint).await?;

            if let Some(spl_stake_pool_program_id) = spl_stake_pool_program_id {
                let (_, spl_stake_pool_state) =
                    get_stake_pool_for_lst_mint(&rpc, &mint, &spl_stake_pool_program_id).await?;

                let in_amount = *arg_matches.get_one::<u64>("amount").unwrap();

                let quote =
                    quote_lst_unstake(&spl_stake_pool_state, &unstake_pool_info, in_amount)?;

                println!(
                    "Quote: {} lamports received for {} {:?} tokens (excluding transaction fees)",
                    quote, in_amount, mint
                );
            } else {
                return Err(anyhow::anyhow!(
                    "Could not find a supported stake pool for the given mint"
                ));
            }
        }
        Some(("quote-unstake-lst-wrapped", arg_matches)) => {
            let rpc = program.rpc();
            let mint: Pubkey =
                Pubkey::from_str(arg_matches.get_one::<String>("mint").unwrap()).unwrap();

            // Make sure to get the right stake pool program id for this mint
            let spl_stake_pool_program_id =
                get_stake_pool_program_for_lst_mint(&rpc, &mint).await?;

            if let Some(spl_stake_pool_program_id) = spl_stake_pool_program_id {
                let (_, spl_stake_pool_state) =
                    get_stake_pool_for_lst_mint(&rpc, &mint, &spl_stake_pool_program_id).await?;

                let in_amount = *arg_matches.get_one::<u64>("amount").unwrap();

                let (quote_wsol, fees) =
                    quote_lst_unstake_wrapped(&spl_stake_pool_state, 
                        &unstake_pool_info, 
                        in_amount, 
                        new_stake_account_as_pda)?;

                println!(
                    "Quote: {} wsol and {} lamports received for {} {:?} tokens",
                    quote_wsol, fees, in_amount, mint
                );
            } else {
                return Err(anyhow::anyhow!(
                    "Could not find a supported stake pool for the given mint"
                ));
            }
        }
        Some(("unstake-lst", arg_matches)) => {
            let rpc = program.rpc();
            let mint: Pubkey =
                Pubkey::from_str(arg_matches.get_one::<String>("mint").unwrap()).unwrap();

            // Make sure to get the right stake pool program id for this mint
            let spl_stake_pool_program_id =
                get_stake_pool_program_for_lst_mint(&rpc, &mint).await?;

            println!("SPL Stake Pool Program ID: {:?}", spl_stake_pool_program_id);

            let amount = arg_matches.get_one::<u64>("amount").unwrap();

            if let Some(spl_stake_pool_program_id) = spl_stake_pool_program_id {
                unstake_lst(
                    &program,
                    &unstake_pool_id,
                    &wallet_keypair,
                    &spl_stake_pool_program_id,
                    &mint,
                    &unstake_pool_info,
                    *amount,
                    simulate,
                    new_stake_account_as_pda,
                )
                .await?;
            } else {
                return Err(anyhow::anyhow!(
                    "Could not find a supported stake pool for the given mint"
                ));
            }
        }
        Some(("unstake-lst-wrapped", arg_matches)) => {
            let rpc = program.rpc();
            let mint: Pubkey =
                Pubkey::from_str(arg_matches.get_one::<String>("mint").unwrap()).unwrap();

            // Make sure to get the right stake pool program id for this mint
            let spl_stake_pool_program_id =
                get_stake_pool_program_for_lst_mint(&rpc, &mint).await?;

            println!("SPL Stake Pool Program ID: {:?}", spl_stake_pool_program_id);

            let amount = arg_matches.get_one::<u64>("amount").unwrap();

            if let Some(spl_stake_pool_program_id) = spl_stake_pool_program_id {
                unstake_lst_wrapped(
                    &program,
                    &unstake_pool_id,
                    &wallet_keypair,
                    &spl_stake_pool_program_id,
                    &mint,
                    &unstake_pool_info,
                    *amount,
                    simulate,
                    new_stake_account_as_pda,
                )
                .await?;
            } else {
                return Err(anyhow::anyhow!(
                    "Could not find a supported stake pool for the given mint"
                ));
            }
        }
        Some(("deposit", arg_matches)) => {
            // Get ATA for the LP token of the unstake pool
            let user_unstake_pool_lp_ata = associated_token::get_associated_token_address(
                &wallet_keypair.pubkey(),
                &unstake_pool_info.lp_mint,
            );

            let lamports = *arg_matches.get_one::<u64>("lamports").unwrap();

            let instructions = program
                .request()
                .accounts(
                    liquid_unstaker::liquid_unstaker::client::accounts::DepositSol {
                        pool: unstake_pool_id,
                        sol_vault: unstake_pool_info.sol_vault,
                        token_program: spl_token::id(),
                        system_program: solana_sdk::system_program::id(),
                        lp_mint: unstake_pool_info.lp_mint,
                        user: wallet_keypair.pubkey(),
                        user_lp_account: user_unstake_pool_lp_ata,
                        associated_token_program: associated_token::ID,
                    },
                )
                .args(liquid_unstaker::liquid_unstaker::client::args::DepositSol {
                    amount: lamports,
                })
                .instructions()?;

            // Build transaction
            let recent_blockhash = program.rpc().get_latest_blockhash().await?;

            let tx = Transaction::new_signed_with_payer(
                &instructions,
                Some(&wallet_keypair.pubkey()),
                &[&wallet_keypair],
                recent_blockhash,
            );

            // Send or simulate the transaction
            send_or_simulate_transaction(&program.rpc(), &tx, simulate, None).await?;
        }
        Some(("withdraw", arg_matches)) => {
            // Get ATA for the LP token of the unstake pool
            let user_unstake_pool_lp_ata = associated_token::get_associated_token_address(
                &wallet_keypair.pubkey(),
                &unstake_pool_info.lp_mint,
            );

            let tokens = *arg_matches.get_one::<u64>("tokens").unwrap();

            let instructions = program
                .request()
                .accounts(
                    liquid_unstaker::liquid_unstaker::client::accounts::WithdrawSol {
                        pool: unstake_pool_id,
                        sol_vault: unstake_pool_info.sol_vault,
                        token_program: spl_token::id(),
                        system_program: solana_sdk::system_program::id(),
                        lp_mint: unstake_pool_info.lp_mint,
                        user: wallet_keypair.pubkey(),
                        user_lp_account: user_unstake_pool_lp_ata,
                    },
                )
                .args(
                    liquid_unstaker::liquid_unstaker::client::args::WithdrawSol {
                        lp_tokens: tokens,
                    },
                )
                .instructions()?;

            // Build transaction
            let recent_blockhash = program.rpc().get_latest_blockhash().await?;

            let tx = Transaction::new_signed_with_payer(
                &instructions,
                Some(&wallet_keypair.pubkey()),
                &[&wallet_keypair],
                recent_blockhash,
            );

            // Send or simulate the transaction
            send_or_simulate_transaction(&program.rpc(), &tx, simulate, None).await?;
        }
        Some(("list-lst-mints", arg_matches)) => {
            let limit = *arg_matches.get_one::<u64>("limit").unwrap_or(&u64::MAX);

            let rpc = program.rpc();

            let mut mints = vec![];

            for program_id in SUPPORTED_STAKE_POOL_PROGRAMS {
                let mints_for_program = get_stake_pool_mints(&rpc, &program_id).await?;
                mints.extend(mints_for_program);
            }

            mints
                .into_iter()
                .take(limit as usize)
                .for_each(|mint| println!("{:?}", mint));
        }
        Some(("pool-info", _arg_matches)) => {

            println!("{:#?}", unstake_pool_info);
        }
        _ => {
            println!("No valid subcommand was provided");
            return Ok(());
        }
    };

    Ok(())
}

async fn unstake_lst(
    program: &anchor_client::Program<Rc<Keypair>>,
    unstake_pool_id: &Pubkey,
    wallet_keypair: &Keypair,
    spl_stake_pool_program_id: &Pubkey,
    mint: &Pubkey,
    unstake_pool_info: &liquid_unstaker::liquid_unstaker::accounts::Pool,
    amount: u64,
    simulate: bool,
    new_stake_account_as_pda: bool,
) -> Result<()> {
    let rpc = program.rpc();

    let (spl_stake_pool_address, spl_stake_pool_state) =
        get_stake_pool_for_lst_mint(&rpc, &mint, &spl_stake_pool_program_id).await?;

    assert_eq!(spl_stake_pool_state.pool_mint, *mint);

    let spl_stake_pool_validator_list = rpc
        .get_account(&spl_stake_pool_state.validator_list)
        .await
        .map(|account| {
            let mut data = account.data.as_slice();
            spl_stake_pool::state::ValidatorList::deserialize(&mut data)
        })??;

    // Get all the accounts we need for liquid unstaking, and calculate amounts to pass to the liquid unstake instruction
    let stake_account_seed= unstake_pool_info.total_deactivating_stake;

    let (lst_amounts, withdraw_stake_accounts, new_stake_accounts, new_stake_pda_accounts) =
        if new_stake_account_as_pda {
        
            get_unstake_accounts_with_new_stake_account_as_pda(
                &spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                stake_account_seed,
                &wallet_keypair.pubkey(),
                amount,
            )?
        } else {
            get_unstake_accounts(
                &spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                amount,
            )?
        };

    let lst_amounts = lst_amounts
        .into_iter()
        .pad_using(5, |_| 0)
        .collect_array::<5>()
        .unwrap();

    let wallet_lst_token_ata = associated_token::get_associated_token_address(
        &wallet_keypair.pubkey(),
        &spl_stake_pool_state.pool_mint,
    );

    let stake_pool_withdraw_authority = Pubkey::find_program_address(
        &[&spl_stake_pool_address.to_bytes(), b"withdraw"],
        &spl_stake_pool_program_id,
    )
    .0;

    let builder = program
        .request()
        .accounts(
            liquid_unstaker::liquid_unstaker::client::accounts::LiquidUnstakeLst {
                pool: *unstake_pool_id,
                sol_vault: unstake_pool_info.sol_vault,
                token_program: spl_token::id(),
                payer: wallet_keypair.pubkey(),
                user_transfer_authority: wallet_keypair.pubkey(),
                user_lst_account: wallet_lst_token_ata,
                user_sol_account: wallet_keypair.pubkey(),
                manager_fee_account: unstake_pool_info.manager_fee_account,
                stake_pool: spl_stake_pool_address,
                stake_pool_validator_list: spl_stake_pool_state.validator_list,
                stake_pool_withdraw_authority: stake_pool_withdraw_authority,
                stake_pool_manager_fee_account: spl_stake_pool_state.manager_fee_account,
                stake_pool_mint: spl_stake_pool_state.pool_mint,
                stake_program: solana_sdk::stake::program::id(),
                stake_pool_program: *spl_stake_pool_program_id,
                system_program: solana_sdk::system_program::id(),
                clock: solana_sdk::sysvar::clock::id(),
                stake_history: solana_sdk::sysvar::stake_history::id(),
            },
        )
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

    let mut instructions = if new_stake_account_as_pda {
        builder
            .args(
                liquid_unstaker::liquid_unstaker::client::args::LiquidUnstakeLstWithSeed {
                    lst_amounts,
                    minimum_lamports_out: None,
                    stake_account_seed: stake_account_seed,
                },
            )
            .instructions()?

    } else {
        builder
            .args(
                liquid_unstaker::liquid_unstaker::client::args::LiquidUnstakeLst {
                    lst_amounts,
                    minimum_lamports_out: None,
                },
            )
            .instructions()?
    };

    // We need to create the new stake accounts before we can send the transaction
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


    instructions.insert(0, solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_000_000));

    // Build transaction
    let recent_blockhash = rpc.get_latest_blockhash().await?;

    let mut signers = vec![wallet_keypair];

    if !new_stake_account_as_pda {
        
        for stake_account in new_stake_accounts.iter() {
            if let PubkeyOrKeypair::Keypair(k) = stake_account {
                signers.push(k);
            } else {
                return Err(anyhow::anyhow!("Expected Keypair for new stake account when using PDA option"));
            }
        }
    }

    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&wallet_keypair.pubkey()),
        &signers,
        recent_blockhash,
    );

    // Send or simulate the transaction
    send_or_simulate_transaction(&program.rpc(), &tx, simulate, 
        Some(vec![
            wallet_keypair.pubkey(), 
            unstake_pool_info.sol_vault,
            unstake_pool_info.manager_fee_account,
            new_stake_accounts[0].pubkey(),
        ])).await?;

    Ok(())
}

async fn unstake_lst_wrapped(
    program: &anchor_client::Program<Rc<Keypair>>,
    unstake_pool_id: &Pubkey,
    wallet_keypair: &Keypair,
    spl_stake_pool_program_id: &Pubkey,
    mint: &Pubkey,
    unstake_pool_info: &liquid_unstaker::liquid_unstaker::accounts::Pool,
    amount: u64,
    simulate: bool,
    new_stake_account_as_pda: bool,
) -> Result<()> {
    let rpc = program.rpc();

    let (spl_stake_pool_address, spl_stake_pool_state) =
        get_stake_pool_for_lst_mint(&rpc, &mint, &spl_stake_pool_program_id).await?;

    assert_eq!(spl_stake_pool_state.pool_mint, *mint);

    let spl_stake_pool_validator_list = rpc
        .get_account(&spl_stake_pool_state.validator_list)
        .await
        .map(|account| {
            let mut data = account.data.as_slice();
            spl_stake_pool::state::ValidatorList::deserialize(&mut data)
        })??;

    // Get all the accounts we need for liquid unstaking, and calculate amounts to pass to the liquid unstake instruction
    let stake_account_seed = unstake_pool_info.total_deactivating_stake;

    let (lst_amounts, withdraw_stake_accounts, new_stake_accounts, new_stake_pda_accounts) =
        if new_stake_account_as_pda {
        
            get_unstake_accounts_with_new_stake_account_as_pda(
                &spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                stake_account_seed,
                &wallet_keypair.pubkey(),
                amount,
            )?
        } else {
            get_unstake_accounts(
                &spl_stake_pool_program_id,
                &spl_stake_pool_address,
                &spl_stake_pool_state,
                &spl_stake_pool_validator_list,
                amount,
            )?
        };

    let lst_amounts = lst_amounts
        .into_iter()
        .pad_using(5, |_| 0)
        .collect_array::<5>()
        .unwrap();

    let wallet_lst_token_ata = associated_token::get_associated_token_address(
        &wallet_keypair.pubkey(),
        &spl_stake_pool_state.pool_mint,
    );

    let wallet_wsol_token_ata = associated_token::get_associated_token_address(
        &wallet_keypair.pubkey(),
        &spl_token::native_mint::id(),
    );

    let stake_pool_withdraw_authority = Pubkey::find_program_address(
        &[&spl_stake_pool_address.to_bytes(), b"withdraw"],
        &spl_stake_pool_program_id,
    )
    .0;

    let builder = program
        .request()
        .accounts(
            liquid_unstaker::liquid_unstaker::client::accounts::LiquidUnstakeLstWithWrapped {
                pool: *unstake_pool_id,
                sol_vault: unstake_pool_info.sol_vault,
                token_program: spl_token::id(),
                payer: wallet_keypair.pubkey(),
                user_transfer_authority: wallet_keypair.pubkey(),
                user_lst_account: wallet_lst_token_ata,
                user_sol_account: wallet_wsol_token_ata,
                manager_fee_account: unstake_pool_info.manager_fee_account,
                stake_pool: spl_stake_pool_address,
                stake_pool_validator_list: spl_stake_pool_state.validator_list,
                stake_pool_withdraw_authority: stake_pool_withdraw_authority,
                stake_pool_manager_fee_account: spl_stake_pool_state.manager_fee_account,
                stake_pool_mint: spl_stake_pool_state.pool_mint,
                stake_program: solana_sdk::stake::program::id(),
                stake_pool_program: *spl_stake_pool_program_id,
                system_program: solana_sdk::system_program::id(),
                clock: solana_sdk::sysvar::clock::id(),
            },
        )
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

    let mut instructions = if new_stake_account_as_pda {

            builder.args(liquid_unstaker::liquid_unstaker::client::args::LiquidUnstakeLstWithWrappedSeed {
                    lst_amounts,
                    minimum_lamports_out: None,
                    stake_account_seed: stake_account_seed
                })
                .instructions()?
        } else {
            builder.args(liquid_unstaker::liquid_unstaker::client::args::LiquidUnstakeLstWithWrapped {
                    lst_amounts,
                    minimum_lamports_out: None,
                })
                .instructions()?
        };
        

    if !new_stake_account_as_pda {
        // We need to create the new stake accounts before we can send the transaction
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

    instructions.insert(0, solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_000_000));

    // Build transaction
    let recent_blockhash = rpc.get_latest_blockhash().await?;

    let mut signers = vec![wallet_keypair];

    if !new_stake_account_as_pda {
        
        for stake_account in new_stake_accounts.iter() {
            if let PubkeyOrKeypair::Keypair(k) = stake_account {
                signers.push(k);
            } else {
                return Err(anyhow::anyhow!("Expected Keypair for new stake account when using PDA option"));
            }
        }
    }

    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&wallet_keypair.pubkey()),
        &signers,
        recent_blockhash,
    );

    // Send or simulate the transaction
    send_or_simulate_transaction(&program.rpc(), &tx, simulate, 
        Some(vec![
            wallet_keypair.pubkey(), 
            wallet_wsol_token_ata,
            unstake_pool_info.sol_vault,
            unstake_pool_info.manager_fee_account,
            new_stake_accounts[0].pubkey(),
        ])).await?;

    Ok(())
}

async fn get_stake_pool_program_for_lst_mint(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> Result<Option<Pubkey>> {
    // Enumerate supported stake pools to find which one the mint belongs to
    for program_id in SUPPORTED_STAKE_POOL_PROGRAMS {
        let mints_for_program = get_stake_pool_mints(&rpc, &program_id).await?;

        if mints_for_program.contains(mint) {
            return Ok(Some(program_id));
        }
    }

    Ok(None)
}

async fn get_stake_pool_mints(rpc: &RpcClient, program_id: &Pubkey) -> Result<Vec<Pubkey>> {
    let spl_stake_pools = rpc
        .get_program_accounts_with_config(
            &program_id,
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
        .map(|(_pubkey, account)| {
            let mut data = account.data.as_slice();
            let pool_state = spl_stake_pool::state::StakePool::deserialize(&mut data).unwrap();

            pool_state.pool_mint
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

        let simulation_accounts_of_interest = simulation_accounts_of_interest.unwrap_or(vec![]);

        let mut pre_simulation_accounts_of_interest_balances = vec![];
        
        for account in simulation_accounts_of_interest.iter() {

            let pre_balance = rpc.get_account(&account).await.ok().map(|a| a.lamports).unwrap_or(0);
            pre_simulation_accounts_of_interest_balances.push((account, pre_balance));
        }

        let result = rpc.simulate_transaction_with_config(
            tx,
            RpcSimulateTransactionConfig {
                accounts: Some(RpcSimulateTransactionAccountsConfig {
                    addresses: simulation_accounts_of_interest.iter().map(|p| p.to_string()).collect_vec(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await?;

        if result.value.err.is_some() {
            println!("Simulation failed: {:#?}", result.value);
        } else {
            println!("Simulation success");

            if let Some(accounts) = result.value.accounts {
                for ((account_key, account_pre_simulation), account_post_simulation) in izip!(pre_simulation_accounts_of_interest_balances, accounts) {
                    if let Some(account_post_simulation) = account_post_simulation {
                        println!("Account {} lamports {} -> {} diff {}", 
                            account_key, 
                            account_pre_simulation, 
                            account_post_simulation.lamports,
                            (account_post_simulation.lamports as i64) - (account_pre_simulation as i64)
                        );
                    }
                }

            }

        }
    } else {
        let result = rpc.send_and_confirm_transaction(tx).await;

        match result {
            Err(err) => {
                println!("Transaction failed: {:#?}", err);
            }
            Ok(signature) => {
                println!("Signature: {:?}", signature);
            }
        }
    }
    Ok(())
}

/// Function to get the SPL Stake Pool info for the given pool (LST) mint, uses the GetProgramAccounts RPC call
async fn get_stake_pool_for_lst_mint(
    rpc: &RpcClient,
    mint: &Pubkey,
    spl_stake_pool_program_id: &Pubkey,
) -> Result<(Pubkey, spl_stake_pool::state::StakePool)> {
    let mut spl_stake_pools = rpc
        .get_program_accounts_with_config(
            &spl_stake_pool_program_id,
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
        return Err(anyhow::anyhow!(
            "Found {} stake pools for the given mint {:?}",
            spl_stake_pools.len(),
            mint
        ));
    }

    let (spl_stake_pool_address, spl_stake_pool_state) = spl_stake_pools.pop().unwrap();

    Ok((spl_stake_pool_address, spl_stake_pool_state))
}

/// Function to get the amount of lamports that would be received by unstaking the given amount of LST tokens
pub fn quote_lst_unstake(
    stake_pool_state: &StakePool,
    liquid_unstake_pool_state: &liquid_unstaker::liquid_unstaker::accounts::Pool,
    pool_tokens: u64,
) -> Result<i64> {
    let pool_tokens_fee = stake_pool_state
        .calc_pool_tokens_stake_withdrawal_fee(pool_tokens)
        .unwrap() as u64;
    let pool_tokens_net = pool_tokens - pool_tokens_fee;
    let total_amount_to_unstake = stake_pool_state
        .calc_lamports_withdraw_amount(pool_tokens_net)
        .unwrap();

    // Since we create the stake account and pay rent for it, the unstake program will give it back to us
    let stake_account_rent = solana_sdk::rent::Rent::default().minimum_balance(
        size_of::<solana_sdk::stake::state::StakeStateV2>(),
    );
    
    let total_amount_to_unstake = total_amount_to_unstake + stake_account_rent;


    if total_amount_to_unstake > liquid_unstake_pool_state.sol_vault_lamports {
        return Err(anyhow::anyhow!(
            "Not enough liquidity in the unstake pool to cover this unstake amount"
        ));
    }

    // Fee is determined by the liquid unstake pool parameters
    let base_fee_pct_bps = Fee::calculate_base_fee(
        &liquid_unstake_pool_state,
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
            .div(100 as u128 * FEE_PCT_DIVISOR as u128) as u64,
    };

    let fee_amount = fee.total_fee();

    let amount_out = total_amount_to_unstake as i64 - fee_amount as i64 - stake_account_rent as i64;

    return Ok(amount_out);
}


/// Function to get the amount of lamports that would be received by unstaking the given amount of LST tokens
pub fn quote_lst_unstake_wrapped(
    stake_pool_state: &StakePool,
    liquid_unstake_pool_state: &liquid_unstaker::liquid_unstaker::accounts::Pool,
    pool_tokens: u64,
    new_stake_account_as_pda: bool,
) -> Result<(i64, i64)> {
    let pool_tokens_fee = stake_pool_state
        .calc_pool_tokens_stake_withdrawal_fee(pool_tokens)
        .unwrap() as u64;
    let pool_tokens_net = pool_tokens - pool_tokens_fee;
    let total_amount_to_unstake = stake_pool_state
        .calc_lamports_withdraw_amount(pool_tokens_net)
        .unwrap();

    // The program will pay rent for a stake account which is included in the fee calculation but
    // deduced from the wsol amount returned to the user
    let stake_account_rent = solana_sdk::rent::Rent::default().minimum_balance(
        size_of::<solana_sdk::stake::state::StakeStateV2>(),
    );
    
    let total_amount_to_unstake = total_amount_to_unstake + stake_account_rent;

    if total_amount_to_unstake > liquid_unstake_pool_state.sol_vault_lamports {
        return Err(anyhow::anyhow!(
            "Not enough liquidity in the unstake pool to cover this unstake amount"
        ));
    }

    // Fee is determined by the liquid unstake pool parameters
    let base_fee_pct_bps = Fee::calculate_base_fee(
        &liquid_unstake_pool_state,
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
            .div(100 as u128 * FEE_PCT_DIVISOR as u128) as u64,
    };

    let fee_amount = fee.total_fee();

    let wsol_amount_out = total_amount_to_unstake as i64 - fee_amount as i64 - stake_account_rent as i64;
    let wsol_amount_out_extra_if_user_payed_for_new_stake_account = if !new_stake_account_as_pda { stake_account_rent as i64 } else { 0 };

    let lamports_amount_out = 0;

    return Ok((wsol_amount_out + wsol_amount_out_extra_if_user_payed_for_new_stake_account, lamports_amount_out));
}

fn get_unstake_accounts(
    stake_pool_program: &Pubkey,
    stake_pool_address: &Pubkey,
    stake_pool_state: &spl_stake_pool::state::StakePool,
    stake_pool_validator_list: &spl_stake_pool::state::ValidatorList,
    amount_in: u64,
) -> Result<(Vec<u64>, Vec<Pubkey>, Vec<PubkeyOrKeypair>, Vec<Pubkey>)> {
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

    // Prepare the list of accounts to withdraw from
    let mut remaining_amount = amount_in;

    let fee = &stake_pool_state.stake_withdrawal_fee;
    let inverse_fee_numerator = fee.denominator - fee.numerator;
    let inverse_fee_denominator = fee.denominator;

    let calc_pool_tokens_for_deposit = |stake_lamports: u64| -> u128 {
        if stake_pool_state.pool_token_supply == 0 || stake_pool_state.total_lamports == 0 {
            return stake_lamports as u128;
        }
        let numerator = stake_lamports as u128 * stake_pool_state.pool_token_supply as u128;

        return numerator / stake_pool_state.total_lamports as u128;
    };

    let mut withdraw_from = Vec::<AccountInfo>::new();

    for is_preferred in [true, false].iter() {
        let filtered_accounts = accounts
            .iter()
            .filter(|a| a.is_preferred == *is_preferred)
            // Sort by lamports descending as we prefer to unstake from the largest stake accounts first
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
        return Err(anyhow::anyhow!("Not enough pool tokens to unstake"));
    }

    withdraw_from.iter().for_each(|account| {
        println!("Withdrawing from stake account {:?} that has {} lamports", account.stake_address, account.lamports);
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
                &liquid_unstaker::liquid_unstaker::ID_CONST,
            )
            .0
        })
        .collect::<Vec<_>>();

    Ok((
        lst_amounts,
        withdraw_stake_accounts,
        new_stake_accounts.into_iter().map(PubkeyOrKeypair::Keypair).collect(),
        new_stake_pda_accounts,
    ))
}


fn get_unstake_accounts_with_new_stake_account_as_pda(
    stake_pool_program: &Pubkey,
    stake_pool_address: &Pubkey,
    stake_pool_state: &spl_stake_pool::state::StakePool,
    stake_pool_validator_list: &spl_stake_pool::state::ValidatorList,
    stake_account_seed: u64,
    token_transfer_authority: &Pubkey,
    amount_in: u64,
) -> Result<(Vec<u64>, Vec<Pubkey>, Vec<PubkeyOrKeypair>, Vec<Pubkey>)> {
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

    // Prepare the list of accounts to withdraw from
    let mut remaining_amount = amount_in;

    let fee = &stake_pool_state.stake_withdrawal_fee;
    let inverse_fee_numerator = fee.denominator - fee.numerator;
    let inverse_fee_denominator = fee.denominator;

    let calc_pool_tokens_for_deposit = |stake_lamports: u64| -> u128 {
        if stake_pool_state.pool_token_supply == 0 || stake_pool_state.total_lamports == 0 {
            return stake_lamports as u128;
        }
        let numerator = stake_lamports as u128 * stake_pool_state.pool_token_supply as u128;

        return numerator / stake_pool_state.total_lamports as u128;
    };

    let mut withdraw_from = Vec::<AccountInfo>::new();

    for is_preferred in [true, false].iter() {
        let filtered_accounts = accounts
            .iter()
            .filter(|a| a.is_preferred == *is_preferred)
            // Sort by lamports descending as we prefer to unstake from the largest stake accounts first
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
        return Err(anyhow::anyhow!("Not enough pool tokens to unstake"));
    }

    withdraw_from.iter().for_each(|account| {
        println!("Withdrawing from stake account {:?} that has {} lamports", account.stake_address, account.lamports);
    });

    let withdraw_stake_accounts = withdraw_from
        .iter()
        .map(|address| address.stake_address)
        .collect_vec();
    let new_stake_accounts = withdraw_from
        .iter()
        .enumerate()
        .map(|(i, _)| {
            // PDA for the new stake account is derived from
            //
            //  1. The payer, which we assume is SwapParams.token_transfer_authority
            //  2. The seed, which is total_deactivating_stake + i
            //
            let (pubkey, _) = Pubkey::find_program_address(
                &[b"stake_account", token_transfer_authority.as_ref(), (stake_account_seed + i as u64).to_le_bytes().as_ref()],
                &liquid_unstaker::liquid_unstaker::ID_CONST,
            );

            println!("Derived new stake account PDA: {}", pubkey);

            pubkey
        })
        .collect::<Vec<_>>();
    let new_stake_pda_accounts = new_stake_accounts
        .iter()
        .map(|stake_account_keypair| {
            Pubkey::find_program_address(
                &[
                    b"stake_account_info",
                    stake_account_keypair.as_ref(),
                ],
                &liquid_unstaker::liquid_unstaker::ID_CONST,
            )
            .0
        })
        .collect::<Vec<_>>();

    Ok((
        lst_amounts,
        withdraw_stake_accounts,
        new_stake_accounts.into_iter().map(PubkeyOrKeypair::Pubkey).collect(),
        new_stake_pda_accounts,
    ))
}
