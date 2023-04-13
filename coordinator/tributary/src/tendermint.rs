use core::ops::Deref;
use std::{sync::Arc, collections::HashMap};

use async_trait::async_trait;

use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use transcript::{Transcript, RecommendedTranscript};

use ciphersuite::{
  group::{
    GroupEncoding,
    ff::{Field, PrimeField},
  },
  Ciphersuite, Ristretto,
};
use schnorr::SchnorrSignature;

use scale::{Encode, Decode};
use tendermint::{
  SignedMessageFor,
  ext::{
    BlockNumber, RoundNumber, Signer as SignerTrait, SignatureScheme, Weights, Block as BlockTrait,
    BlockError as TendermintBlockError, Commit, Network as NetworkTrait,
  },
};

use tokio::time::{Duration, sleep};

use crate::{ReadWrite, Transaction, TransactionError, BlockHeader, Block, BlockError, Blockchain};

fn challenge(
  genesis: [u8; 32],
  key: [u8; 32],
  nonce: &[u8],
  msg: &[u8],
) -> <Ristretto as Ciphersuite>::F {
  let mut transcript = RecommendedTranscript::new(b"Tributary Chain Tendermint Message");
  transcript.append_message(b"genesis", genesis);
  transcript.append_message(b"key", key);
  transcript.append_message(b"nonce", nonce);
  transcript.append_message(b"message", msg);

  <Ristretto as Ciphersuite>::F::from_bytes_mod_order_wide(&transcript.challenge(b"schnorr").into())
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Signer {
  genesis: [u8; 32],
  key: Zeroizing<<Ristretto as Ciphersuite>::F>,
}

#[async_trait]
impl SignerTrait for Signer {
  type ValidatorId = [u8; 32];
  type Signature = [u8; 64];

  /// Returns the validator's current ID. Returns None if they aren't a current validator.
  async fn validator_id(&self) -> Option<Self::ValidatorId> {
    Some((Ristretto::generator() * self.key.deref()).to_bytes())
  }

  /// Sign a signature with the current validator's private key.
  async fn sign(&self, msg: &[u8]) -> Self::Signature {
    let mut nonce = Zeroizing::new(RecommendedTranscript::new(b"Tributary Chain Tendermint Nonce"));
    nonce.append_message(b"genesis", self.genesis);
    nonce.append_message(b"key", Zeroizing::new(self.key.deref().to_repr()).as_ref());
    nonce.append_message(b"message", msg);
    let mut nonce = nonce.challenge(b"nonce");

    let mut nonce_arr = [0; 64];
    nonce_arr.copy_from_slice(nonce.as_ref());

    let nonce_ref: &mut [u8] = nonce.as_mut();
    nonce_ref.zeroize();
    let nonce_ref: &[u8] = nonce.as_ref();
    assert_eq!(nonce_ref, [0; 64].as_ref());

    let nonce =
      Zeroizing::new(<Ristretto as Ciphersuite>::F::from_bytes_mod_order_wide(&nonce_arr));
    nonce_arr.zeroize();

    assert!(!bool::from(nonce.ct_eq(&<Ristretto as Ciphersuite>::F::ZERO)));

    let challenge = challenge(
      self.genesis,
      (Ristretto::generator() * self.key.deref()).to_bytes(),
      (Ristretto::generator() * nonce.deref()).to_bytes().as_ref(),
      msg,
    );

    let sig = SchnorrSignature::<Ristretto>::sign(&self.key, nonce, challenge).serialize();

    let mut res = [0; 64];
    res.copy_from_slice(&sig);
    res
  }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Validators {
  genesis: [u8; 32],
  weight: u64,
  weights: HashMap<[u8; 32], u64>,
  robin: Vec<[u8; 32]>,
}

impl SignatureScheme for Validators {
  type ValidatorId = [u8; 32];
  type Signature = [u8; 64];
  // TODO: Use half-aggregation.
  type AggregateSignature = Vec<[u8; 64]>;
  type Signer = Arc<Signer>;

  #[must_use]
  fn verify(&self, validator: Self::ValidatorId, msg: &[u8], sig: &Self::Signature) -> bool {
    if !self.weights.contains_key(&validator) {
      return false;
    }
    let Ok(validator_point) = Ristretto::read_G::<&[u8]>(&mut validator.as_ref()) else {
      return false;
    };
    let Ok(actual_sig) = SchnorrSignature::<Ristretto>::read::<&[u8]>(&mut sig.as_ref()) else {
      return false;
    };
    actual_sig.verify(validator_point, challenge(self.genesis, validator, &sig[.. 32], msg))
  }

  fn aggregate(sigs: &[Self::Signature]) -> Self::AggregateSignature {
    sigs.to_vec()
  }

  #[must_use]
  fn verify_aggregate(
    &self,
    signers: &[Self::ValidatorId],
    msg: &[u8],
    sig: &Self::AggregateSignature,
  ) -> bool {
    for (signer, sig) in signers.iter().zip(sig.iter()) {
      if !self.verify(*signer, msg, sig) {
        return false;
      }
    }
    true
  }
}

impl Weights for Validators {
  type ValidatorId = [u8; 32];

  fn total_weight(&self) -> u64 {
    self.weight
  }
  fn weight(&self, validator: Self::ValidatorId) -> u64 {
    self.weights[&validator]
  }
  fn proposer(&self, block: BlockNumber, round: RoundNumber) -> Self::ValidatorId {
    let block = usize::try_from(block.0).unwrap();
    let round = usize::try_from(round.0).unwrap();
    // If multiple rounds are used, a naive block + round would cause the same index to be chosen
    // in quick succesion.
    // Accordingly, if we use additional rounds, jump halfway around.
    // While this is still game-able, it's not explicitly reusing indexes immediately after each
    // other.
    self.robin
      [(block + (if round == 0 { 0 } else { round + (self.robin.len() / 2) })) % self.robin.len()]
  }
}

#[derive(Clone, PartialEq, Eq, Debug, Encode, Decode)]
struct TendermintBlock(Vec<u8>);
impl BlockTrait for TendermintBlock {
  type Id = [u8; 32];
  fn id(&self) -> Self::Id {
    BlockHeader::read::<&[u8]>(&mut self.0.as_ref()).unwrap().hash()
  }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Network<T: Transaction> {
  genesis: [u8; 32],
  signer: Arc<Signer>,
  validators: Arc<Validators>,
  blockchain: Blockchain<T>,
}

#[async_trait]
impl<T: Transaction> NetworkTrait for Network<T> {
  type ValidatorId = [u8; 32];
  type SignatureScheme = Arc<Validators>;
  type Weights = Arc<Validators>;
  type Block = TendermintBlock;

  const BLOCK_PROCESSING_TIME: u32 = 3;
  const LATENCY_TIME: u32 = 1;

  fn signer(&self) -> Arc<Signer> {
    self.signer.clone()
  }
  fn signature_scheme(&self) -> Arc<Validators> {
    self.validators.clone()
  }
  fn weights(&self) -> Arc<Validators> {
    self.validators.clone()
  }

  async fn broadcast(&mut self, _msg: SignedMessageFor<Self>) {
    todo!()
  }
  async fn slash(&mut self, validator: Self::ValidatorId) {
    log::error!(
      "validator {} was slashed on tributary {}",
      hex::encode(validator),
      hex::encode(self.genesis)
    );
  }

  async fn validate(&mut self, block: &Self::Block) -> Result<(), TendermintBlockError> {
    let block =
      Block::read::<&[u8]>(&mut block.0.as_ref()).map_err(|_| TendermintBlockError::Fatal)?;
    self.blockchain.verify_block(&block).map_err(|e| match e {
      BlockError::TransactionError(TransactionError::MissingProvided(_)) => {
        TendermintBlockError::Temporal
      }
      _ => TendermintBlockError::Fatal,
    })
  }

  async fn add_block(
    &mut self,
    block: Self::Block,
    _commit: Commit<Self::SignatureScheme>,
  ) -> Option<Self::Block> {
    let invalid_block = || {
      // There's a fatal flaw in the code, it's behind a hard fork, or the validators turned
      // malicious
      // All justify a halt to then achieve social consensus from
      // TODO: Under multiple validator sets, a small validator set turning malicious knocks
      // off the entire network. That's an unacceptable DoS.
      panic!("validators added invalid block to tributary {}", hex::encode(self.genesis));
    };

    let Ok(block) = Block::read::<&[u8]>(&mut block.0.as_ref()) else {
      return invalid_block();
    };

    loop {
      match self.blockchain.add_block(&block) {
        Ok(()) => break,
        Err(BlockError::TransactionError(TransactionError::MissingProvided(hash))) => {
          log::error!(
            "missing provided transaction {} which other validators on tributary {} had",
            hex::encode(hash),
            hex::encode(self.genesis)
          );
          sleep(Duration::from_secs(30)).await;
        }
        _ => return invalid_block(),
      }
    }

    // TODO: Handle the commit and return the next proposal
    todo!()
  }
}