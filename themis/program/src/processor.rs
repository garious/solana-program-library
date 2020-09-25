//! Themis program
use crate::{
    error::ThemisError,
    instruction::ThemisInstruction,
    state::{Policies, User},
};
use curve25519_dalek::{
    ristretto::RistrettoPoint,
    scalar::Scalar,
};
use elgamal_ristretto::public::PublicKey;
use solana_sdk::{
    account_info::{next_account_info, AccountInfo},
    program_error::ProgramError,
    pubkey::Pubkey,
};

fn process_initialize_user_account(user_info: &AccountInfo) -> Result<(), ProgramError> {
    let mut user = User::deserialize(&user_info.data.borrow()).unwrap_or_default();
    if user.is_initialized {
        return Err(ThemisError::AccountInUse.into());
    }
    user.is_initialized = true;
    user.serialize(&mut user_info.data.borrow_mut())
}

fn process_initialize_policies_account(
    scalars: Vec<Scalar>,
    policies_info: &AccountInfo,
) -> Result<(), ProgramError> {
    let mut policies = Policies::deserialize(&policies_info.data.borrow()).unwrap_or_default();
    if policies.is_initialized {
        return Err(ThemisError::AccountInUse.into());
    }
    policies.is_initialized = true;
    policies.scalars = scalars;
    policies.serialize(&mut policies_info.data.borrow_mut())
}

fn process_calculate_aggregate(
    encrypted_interactions: &[(RistrettoPoint, RistrettoPoint)],
    public_key: PublicKey,
    user_info: &AccountInfo,
    policies_info: &AccountInfo,
) -> Result<(), ProgramError> {
    let mut user = User::deserialize(&user_info.data.borrow())?;
    let policies = Policies::deserialize(&policies_info.data.borrow())?;
    user.calculate_aggregate(
        encrypted_interactions,
        public_key.get_point(),
        &policies.scalars,
    );
    user.serialize(&mut user_info.data.borrow_mut())
}

fn process_submit_proof_decryption(
    plaintext: RistrettoPoint,
    announcement_g: RistrettoPoint,
    announcement_ctx: RistrettoPoint,
    response: Scalar,
    user_info: &AccountInfo,
) -> Result<(), ProgramError> {
    let mut user = User::deserialize(&user_info.data.borrow())?;
    user.submit_proof_decryption(plaintext, announcement_g, announcement_ctx, response);
    user.serialize(&mut user_info.data.borrow_mut())
}

fn process_request_payment(
    encrypted_aggregate: (RistrettoPoint, RistrettoPoint),
    decrypted_aggregate: RistrettoPoint,
    proof_correct_decryption: RistrettoPoint,
    user_info: &AccountInfo,
) -> Result<(), ProgramError> {
    let mut user = User::deserialize(&user_info.data.borrow())?;
    user.request_payment(
        encrypted_aggregate,
        decrypted_aggregate,
        proof_correct_decryption,
    );
    user.serialize(&mut user_info.data.borrow_mut())
}

/// Process the given transaction instruction
pub fn process_instruction<'a>(
    _program_id: &Pubkey,
    account_infos: &'a [AccountInfo<'a>],
    input: &[u8],
) -> Result<(), ProgramError> {
    let account_infos_iter = &mut account_infos.iter();
    let instruction = ThemisInstruction::deserialize(input)?;

    match instruction {
        ThemisInstruction::InitializeUserAccount => {
            let user_info = next_account_info(account_infos_iter)?;
            process_initialize_user_account(&user_info)
        }
        ThemisInstruction::InitializePoliciesAccount { scalars } => {
            let policies_info = next_account_info(account_infos_iter)?;
            process_initialize_policies_account(scalars, &policies_info)
        }
        ThemisInstruction::CalculateAggregate {
            encrypted_interactions,
            public_key,
        } => {
            let user_info = next_account_info(account_infos_iter)?;
            let policies_info = next_account_info(account_infos_iter)?;
            process_calculate_aggregate(
                &encrypted_interactions,
                public_key,
                &user_info,
                &policies_info,
            )
        }
        ThemisInstruction::SubmitProofDecryption {
            plaintext,
            announcement_g,
            announcement_ctx,
            response,
        } => {
            let user_info = next_account_info(account_infos_iter)?;
            process_submit_proof_decryption(
                plaintext,
                announcement_g,
                announcement_ctx,
                response,
                &user_info,
            )
        }
        ThemisInstruction::RequestPayment {
            encrypted_aggregate,
            decrypted_aggregate,
            proof_correct_decryption,
        } => {
            let user_info = next_account_info(account_infos_iter)?;
            process_request_payment(
                *encrypted_aggregate,
                decrypted_aggregate,
                proof_correct_decryption,
                &user_info,
            )
        }
    }
}
