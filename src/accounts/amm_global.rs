use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::pubkey::Pubkey;

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, Default)]
pub struct AMMGlobalAccount {
    pub discriminator: u64,
    pub admin: Pubkey,
    pub lp_fee_basis_points: u64,
    pub protocol_fee_basis_points: u64,
    pub disable_flags: u8,
    pub protocol_fee_recipients: [Pubkey; 8],
}
