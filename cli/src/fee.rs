use anyhow::Result;

pub struct Fee {
    pub base_fee: u64,
    pub manager_fee: u64,
}

pub const FEE_PCT_BPS: u32 = 100_000;


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
            .map_err(|_| anyhow::anyhow!("Overflow"))
    }

    pub fn calculate_base_fee(
        pool: &liquid_unstaker::liquid_unstaker::accounts::Pool, 
        current_sol_vault_lamports: u64, 
        _unstake_lamports: u64) -> Result<u64> {

        if current_sol_vault_lamports >= pool.min_sol_for_min_fee {
            // We will stay above the minimum fee threshold, the base fee is always equal to fee_min
            Ok(pool.fee_min.into())
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