use anchor_lang::prelude::*;

#[error_code]
pub enum LiquidUnstakerErrorCode {
    #[msg("Insufficient SOL in the vault")]
    InsufficientSolVaultBalance,
    #[msg("Math operation overflow")]
    MathOverflow,
    #[msg("Math operation underflow")]
    MathUnderflow,
    #[msg("No fees to claim.")]
    NoFeesToClaim,
    #[msg("Failed to withdraw from SPL Stake Pool")]
    StakePoolWithdrawalFailed,
    #[msg("Failed to set authority on stake account")]
    SetAuthorityFailed,
    #[msg("Failed to deactivate stake account.")]
    DeactivateStakeFailed,
    #[msg("InvalidWithdrawAuthority")]
    InvalidWithdrawAuthority,
    #[msg("Invalid stake account owner")]
    InvalidStakeAccountOwner,
    #[msg("Invalid stake account state")]
    InvalidStakeAccountState,
    #[msg("Unauthorized stake account")]
    UnauthorizedStakeAccount,
    #[msg("Stake account has already been processed.")]
    StakeAccountAlreadyProcessed,    
    #[msg("Stake accounts mismatch")]
    StakeAccountMismatch,
    #[msg("Failed to deserialize")]
    FailedToDeserialize,
    #[msg("Remaining accounts passed are not valid")]
    InvalidRemainingAccounts,
    #[msg("Unable to load the lockup information of the stake account")]
    StakeAccountLockupUnableToLoadLockup,
    #[msg("The lockup of the stake account is in force")]
    StakeAccountLockupIsInForce,
    #[msg("The stake account is not yet fully deactivated")]
    StakeAccountNotFullyDeactivated,
    #[msg("Unsppoorted stake pool program")]
    InvalidStakePoolProgram,
    #[msg("Insufficient LP tokens")]
    InsufficientLpTokenBalance,
    #[msg("Must deposit more than 0 lamports")]
    DepositMustBeLargerThanZero,
    #[msg("Invalid user LP account")]
    InvalidUserLpAccount,
    #[msg("Uanble to mint any LP tokens as the amount calculated is zero")]
    LpTokensToMintIsZero,
    #[msg("Stake account does not belong to pool")]
    StakeAccountDoesNotBelongToPool,
    #[msg("fee_max cannot be lower than fee_min")]
    FeeMaxLessThanFeeMin,
    #[msg("fee_max is set to a too high value")]
    FeeMaxTooHigh,
    #[msg("manager_fee_pct is set to a too high value")]
    ManagerFeePctTooHigh,
    #[msg("Metadata account address is incorrect")]
    IncorrectMetadataAccount,
    #[msg("The cap has been reached for the pool's SOL vault")]
    SolVaultLamportsCapReached,
    #[msg("The slippage was exceeded")]
    SlippageExceeded,
    #[msg("Stake split failed")]
    SplitStakeFailed,
    #[msg("The amount to withdraw to the stake account is incorrect, e.g. has to be at leats 1 SOL and the remaining balance must also be at least 1 SOL")]
    WithdrawToStakeIncorrectAmount,
    #[msg("Flash loan not repaid in the same transaction")]
    FlashLoanNotRepaidInSameTransaction,
    #[msg("Flash loan repayment amount is insufficient")]
    FlashLoanRepaymentAmountInsufficient,
    #[msg("Flash loan amount exceeds available liquidity")]
    FlashLoanExceedsAvailableLiquidity,
    #[msg("Flash borrow already active - cannot borrow again without repaying")]
    FlashBorrowAlreadyActive,
    #[msg("No active flash loan to repay")]
    NoActiveFlashLoan,
    #[msg("Flash repay instruction not found in transaction")]
    FlashRepayInstructionNotFound,
    #[msg("Invalid flash repay instruction accounts")]
    InvalidFlashRepayInstruction,
    #[msg("Flash loans are disabled for this pool")]
    FlashLoansDisabled,
    #[msg("Flash loan instructions cannot be called via CPI")]
    FlashLoanCpiNotAllowed,
    #[msg("Cannot deposit or withdraw LP tokens while flash loan is active")]
    FlashLoanActive,
    #[msg("Invalid flash loan fee configuration")]
    InvalidFlashLoanFee,
    #[msg("Invalid withdraw fee configuration")]
    InvalidWithdrawFee,
    #[msg("Invalid stake account info PDA address")]
    InvalidStakeAccountInfoAddress,
}
