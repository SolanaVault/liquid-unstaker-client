use anchor_lang::prelude::*;
use liquid_unstaker::liquid_unstaker::accounts::Pool;

use crate::error::LiquidUnstakerErrorCode;

pub const FEE_PCT_DIVISOR: u32 = 100_000;

pub struct Fee {
    pub base_fee: u64,
    pub manager_fee: u64,
}

impl Fee {

    /// REturn the total fee, base fee + manager fee
    pub fn total_fee(&self) -> u64 {
        self.base_fee + self.manager_fee
    }

    fn calculate_linear_base_fee(amount: u64, numerator: u64, denominator: u64) -> Result<u64> {
        
        if denominator == 0 {
            return Ok(amount);
        }

        u64::try_from((amount as u128) * (numerator as u128) / (denominator as u128))
            .map_err(|_| error!(LiquidUnstakerErrorCode::MathOverflow))
    }

    pub fn calculate_base_fee(pool: &Pool, current_sol_vault_lamports: u64, unstake_lamports: u64) -> Result<u64> {

        let remaining_lamports = current_sol_vault_lamports
            .checked_sub(unstake_lamports)
            .ok_or(LiquidUnstakerErrorCode::MathUnderflow)?;

        if remaining_lamports >= pool.min_sol_for_min_fee {
            // We will stay above the minimum fee threshold, the base fee is always equal to fee_min
            Ok(pool.fee_min.into())
        } else {

            // If the unstake will push us below the minimum fee threshold, we need to calculate the fee in two
            // parts: the part above the minimum fee threshold which will be charged fee_min, and the part below
            // the minimum fee threshold which will be charged a linear fee between fee_min and fee_max
            if current_sol_vault_lamports >= pool.min_sol_for_min_fee {
                let amount_above_min_fee_threshold = current_sol_vault_lamports - pool.min_sol_for_min_fee;
                let amount_below_min_fee_threshold = unstake_lamports - amount_above_min_fee_threshold;

                Ok((pool.fee_min as u64)
                    .checked_mul(amount_above_min_fee_threshold)
                    .ok_or(LiquidUnstakerErrorCode::MathOverflow)?
                    .checked_add(Fee::calculate_linear_base_fee(
                        (pool.fee_max - pool.fee_min).into(),
                        amount_below_min_fee_threshold, 
                        pool.min_sol_for_min_fee)?
                        .checked_add(pool.fee_min.into())
                        .ok_or(LiquidUnstakerErrorCode::MathOverflow)?
                        .checked_mul(amount_below_min_fee_threshold)
                        .ok_or(LiquidUnstakerErrorCode::MathOverflow)?
                    )
                    .ok_or(LiquidUnstakerErrorCode::MathOverflow)?
                    .checked_div(unstake_lamports)
                    .ok_or(LiquidUnstakerErrorCode::MathUnderflow)?)
            } else {

                // We will fall below the minimum fee threshold, calculate a linear fee between fee_min and fee_max, depending
                // on how much SOL is left in the vault. The lower the amount of SOL in the vault, the higher the fee should be
                Ok(Fee::calculate_linear_base_fee(
                    (pool.fee_max - pool.fee_min).into(),
                    pool.min_sol_for_min_fee - current_sol_vault_lamports, 
                    pool.min_sol_for_min_fee)? + pool.fee_min as u64)
            }
        }
    }

}

