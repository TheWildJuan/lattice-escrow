use anchor_lang::prelude::*;
use anchor_lang::system_program;

declare_id!("11111111111111111111111111111111"); // replaced on deploy

#[program]
pub mod escrow {
    use super::*;

    /// Called by the user's agent to lock funds for a transaction
    pub fn create_escrow(
        ctx: Context<CreateEscrow>,
        transaction_id: String,
        amount_lamports: u64,
        timeout_seconds: i64,
    ) -> Result<()> {
        let clock      = Clock::get()?;
        let payer_key  = ctx.accounts.payer.key();
        let agent_key  = ctx.accounts.agent.key();
        let wallet_key = ctx.accounts.lattice_wallet.key();
        let expires    = clock.unix_timestamp + timeout_seconds;
        let fee        = amount_lamports / 67;
        let bump       = ctx.bumps.escrow_account;
        let tx_id      = transaction_id.clone();

        // Set escrow fields
        {
            let escrow             = &mut ctx.accounts.escrow_account;
            escrow.transaction_id  = transaction_id;
            escrow.payer           = payer_key;
            escrow.agent           = agent_key;
            escrow.lattice_wallet  = wallet_key;
            escrow.amount          = amount_lamports;
            escrow.fee             = fee;
            escrow.status          = EscrowStatus::Pending;
            escrow.created_at      = clock.unix_timestamp;
            escrow.expires_at      = expires;
            escrow.bump            = bump;
        }

        // Transfer SOL from payer into escrow PDA
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to:   ctx.accounts.escrow_account.to_account_info(),
                },
            ),
            amount_lamports,
        )?;

        emit!(EscrowCreated {
            transaction_id: tx_id,
            payer:          payer_key,
            agent:          agent_key,
            amount:         amount_lamports,
            expires_at:     expires,
        });

        Ok(())
    }

    /// Called by Lattice when the agent confirms delivery
    /// Releases funds to the agent (minus fee to Lattice)
    pub fn release_escrow(ctx: Context<ReleaseEscrow>) -> Result<()> {
        let clock = Clock::get()?;

        // Read all values before mutating
        let status      = ctx.accounts.escrow_account.status.clone();
        let expires_at  = ctx.accounts.escrow_account.expires_at;
        let amount      = ctx.accounts.escrow_account.amount;
        let fee         = ctx.accounts.escrow_account.fee;
        let tx_id       = ctx.accounts.escrow_account.transaction_id.clone();
        let agent_key   = ctx.accounts.agent.key();

        require!(status == EscrowStatus::Pending, EscrowError::AlreadySettled);
        require!(clock.unix_timestamp <= expires_at, EscrowError::Expired);

        let agent_amount = amount - fee;

        // Mutate status
        ctx.accounts.escrow_account.status = EscrowStatus::Released;

        // Transfer lamports
        **ctx.accounts.escrow_account.to_account_info().lamports.borrow_mut() -= agent_amount + fee;
        **ctx.accounts.agent.to_account_info().lamports.borrow_mut()          += agent_amount;
        **ctx.accounts.lattice_wallet.to_account_info().lamports.borrow_mut() += fee;

        emit!(EscrowReleased {
            transaction_id: tx_id,
            agent:          agent_key,
            amount:         agent_amount,
            fee,
        });

        Ok(())
    }

    /// Called by Lattice when timeout expires or agent fails to deliver
    /// Full refund to payer
    pub fn refund_escrow(ctx: Context<RefundEscrow>) -> Result<()> {
        let clock = Clock::get()?;

        let status      = ctx.accounts.escrow_account.status.clone();
        let expires_at  = ctx.accounts.escrow_account.expires_at;
        let amount      = ctx.accounts.escrow_account.amount;
        let lattice_key = ctx.accounts.escrow_account.lattice_wallet;
        let tx_id       = ctx.accounts.escrow_account.transaction_id.clone();
        let payer_key   = ctx.accounts.payer.key();

        require!(status == EscrowStatus::Pending, EscrowError::AlreadySettled);

        let expired      = clock.unix_timestamp > expires_at;
        let is_authority = ctx.accounts.authority.key() == lattice_key;
        require!(expired || is_authority, EscrowError::NotExpiredYet);

        ctx.accounts.escrow_account.status = EscrowStatus::Refunded;

        **ctx.accounts.escrow_account.to_account_info().lamports.borrow_mut() -= amount;
        **ctx.accounts.payer.to_account_info().lamports.borrow_mut()          += amount;

        emit!(EscrowRefunded {
            transaction_id: tx_id,
            payer:          payer_key,
            amount,
        });

        Ok(())
    }
}

// ─── ACCOUNTS ────────────────────────────────────────────────────────────────

#[derive(Accounts)]
#[instruction(transaction_id: String)]
pub struct CreateEscrow<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// CHECK: agent's wallet — verified in Lattice API before this call
    pub agent: AccountInfo<'info>,

    /// CHECK: Lattice escrow wallet
    pub lattice_wallet: AccountInfo<'info>,

    #[account(
        init,
        payer = payer,
        space = EscrowAccount::LEN,
        seeds = [b"escrow", payer.key().as_ref(), transaction_id.as_bytes()],
        bump
    )]
    pub escrow_account: Account<'info, EscrowAccount>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ReleaseEscrow<'info> {
    /// CHECK: Lattice authority signs the release
    #[account(mut)]
    pub authority: Signer<'info>,

    /// CHECK: agent receives funds
    #[account(mut)]
    pub agent: AccountInfo<'info>,

    /// CHECK: Lattice fee wallet
    #[account(mut)]
    pub lattice_wallet: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"escrow", escrow_account.payer.as_ref(), escrow_account.transaction_id.as_bytes()],
        bump = escrow_account.bump,
        constraint = escrow_account.agent == agent.key() @ EscrowError::WrongAgent,
        constraint = escrow_account.lattice_wallet == authority.key() @ EscrowError::Unauthorized,
    )]
    pub escrow_account: Account<'info, EscrowAccount>,
}

#[derive(Accounts)]
pub struct RefundEscrow<'info> {
    /// CHECK: can be the payer or Lattice authority
    #[account(mut)]
    pub authority: Signer<'info>,

    /// CHECK: payer gets refunded
    #[account(mut)]
    pub payer: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"escrow", escrow_account.payer.as_ref(), escrow_account.transaction_id.as_bytes()],
        bump = escrow_account.bump,
        constraint = escrow_account.payer == payer.key() @ EscrowError::WrongPayer,
    )]
    pub escrow_account: Account<'info, EscrowAccount>,
}

// ─── STATE ────────────────────────────────────────────────────────────────────

#[account]
pub struct EscrowAccount {
    pub transaction_id: String,   // 64 chars max
    pub payer:          Pubkey,
    pub agent:          Pubkey,
    pub lattice_wallet: Pubkey,
    pub amount:         u64,
    pub fee:            u64,
    pub status:         EscrowStatus,
    pub created_at:     i64,
    pub expires_at:     i64,
    pub bump:           u8,
}

impl EscrowAccount {
    pub const LEN: usize =
        8           // discriminator
        + 4 + 64    // transaction_id string
        + 32        // payer
        + 32        // agent
        + 32        // lattice_wallet
        + 8         // amount
        + 8         // fee
        + 1         // status enum
        + 8         // created_at
        + 8         // expires_at
        + 1;        // bump
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
pub enum EscrowStatus {
    Pending,
    Released,
    Refunded,
}

// ─── EVENTS ───────────────────────────────────────────────────────────────────

#[event]
pub struct EscrowCreated {
    pub transaction_id: String,
    pub payer:          Pubkey,
    pub agent:          Pubkey,
    pub amount:         u64,
    pub expires_at:     i64,
}

#[event]
pub struct EscrowReleased {
    pub transaction_id: String,
    pub agent:          Pubkey,
    pub amount:         u64,
    pub fee:            u64,
}

#[event]
pub struct EscrowRefunded {
    pub transaction_id: String,
    pub payer:          Pubkey,
    pub amount:         u64,
}

// ─── ERRORS ───────────────────────────────────────────────────────────────────

#[error_code]
pub enum EscrowError {
    #[msg("Escrow already settled")]
    AlreadySettled,
    #[msg("Escrow has expired")]
    Expired,
    #[msg("Escrow has not expired yet")]
    NotExpiredYet,
    #[msg("Unauthorized — must be Lattice authority")]
    Unauthorized,
    #[msg("Wrong agent account")]
    WrongAgent,
    #[msg("Wrong payer account")]
    WrongPayer,
}
