use solana_sdk::{
    instruction::CompiledInstruction, message::SanitizedMessage, pubkey::Pubkey,
    signature::Signature, transaction::SanitizedTransaction,
};

pub trait Message {
    /// Return the number of signatures in the message.
    fn num_signatures(&self) -> u64;

    /// Return the number of writeable accounts in the message.
    fn num_write_locks(&self) -> u64;

    /// Return the number of instructions in the message.
    fn num_instructions(&self) -> usize;

    /// Return an iterator over the instructions in the message.
    fn instructions_iter(&self) -> impl Iterator<Item = Instruction>;

    /// Return an iterator over the instructions in the message, paired with
    /// the pubkey of the program.
    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)>;

    /// Returns `true` if the account at `index` is writable.
    fn is_writable(&self, index: usize) -> bool;

    /// Returns `true` if the account at `index` is signer.
    fn is_signer(&self, index: usize) -> bool;
}

pub trait SignedMessage: Message {
    /// Get the first signature of the message.
    fn signature(&self) -> &Signature;

    /// Get all the signatures of the message.
    fn signatures(&self) -> &[Signature];
}

/// A non-owning version of [`CompiledInstruction`] that references
/// slices of account indexes and data
///
/// [`Message`]: crate::message::Message
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Instruction<'a> {
    /// Index into the transaction keys array indicating the program account that executes this instruction.
    pub program_id_index: u8,
    /// Ordered indices into the transaction keys array indicating which accounts to pass to the program.
    pub accounts: &'a [u8],
    /// The program input data.
    pub data: &'a [u8],
}

// Implement for the "reference" `SanitizedMessage` type.
impl Message for SanitizedMessage {
    fn num_signatures(&self) -> u64 {
        SanitizedMessage::num_signatures(self)
    }

    fn num_write_locks(&self) -> u64 {
        SanitizedMessage::num_write_locks(self)
    }

    fn num_instructions(&self) -> usize {
        self.instructions().len()
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        self.instructions().iter().map(Instruction::from)
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        SanitizedMessage::program_instructions_iter(self)
            .map(|(pubkey, ix)| (pubkey, Instruction::from(ix)))
    }

    fn is_writable(&self, index: usize) -> bool {
        SanitizedMessage::is_writable(self, index)
    }

    fn is_signer(&self, index: usize) -> bool {
        SanitizedMessage::is_signer(self, index)
    }
}

impl Message for SanitizedTransaction {
    fn num_signatures(&self) -> u64 {
        Message::num_signatures(self.message())
    }

    fn num_write_locks(&self) -> u64 {
        Message::num_write_locks(self.message())
    }

    fn num_instructions(&self) -> usize {
        Message::num_instructions(self.message())
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        Message::instructions_iter(self.message())
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        Message::program_instructions_iter(self.message())
    }

    fn is_writable(&self, index: usize) -> bool {
        Message::is_writable(self.message(), index)
    }

    fn is_signer(&self, index: usize) -> bool {
        Message::is_signer(self.message(), index)
    }
}

impl SignedMessage for SanitizedTransaction {
    fn signature(&self) -> &Signature {
        self.signatures().first().unwrap()
    }

    fn signatures(&self) -> &[Signature] {
        self.signatures()
    }
}

impl<'a> From<&'a CompiledInstruction> for Instruction<'a> {
    fn from(ix: &'a CompiledInstruction) -> Self {
        Self {
            program_id_index: ix.program_id_index,
            accounts: ix.accounts.as_slice(),
            data: ix.data.as_slice(),
        }
    }
}
