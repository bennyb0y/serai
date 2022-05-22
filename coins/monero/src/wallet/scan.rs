use std::convert::TryFrom;

use curve25519_dalek::{
  constants::ED25519_BASEPOINT_TABLE,
  scalar::Scalar,
  edwards::EdwardsPoint
};

use monero::{consensus::deserialize, blockdata::transaction::ExtraField};

use crate::{
  Commitment,
  serialize::write_varint,
  transaction::Transaction,
  wallet::{uniqueness, shared_key, amount_decryption, commitment_mask}
};

#[derive(Clone, Debug)]
pub struct SpendableOutput {
  pub tx: [u8; 32],
  pub o: usize,
  pub key: EdwardsPoint,
  pub key_offset: Scalar,
  pub commitment: Commitment
}

// TODO: Enable disabling one of the shared key derivations and solely using one
// Change outputs currently always use unique derivations, so that must also be corrected
impl Transaction {
  pub fn scan(
    &self,
    view: Scalar,
    spend: EdwardsPoint
  ) -> Vec<SpendableOutput> {
    let mut extra = vec![];
    write_varint(&u64::try_from(self.prefix.extra.len()).unwrap(), &mut extra).unwrap();
    extra.extend(&self.prefix.extra);
    let extra = deserialize::<ExtraField>(&extra);

    let pubkeys: Vec<EdwardsPoint>;
    if let Ok(extra) = extra {
      let mut m_pubkeys = vec![];
      if let Some(key) = extra.tx_pubkey() {
        m_pubkeys.push(key);
      }
      if let Some(keys) = extra.tx_additional_pubkeys() {
        m_pubkeys.extend(&keys);
      }

      pubkeys = m_pubkeys.iter().map(|key| key.point.decompress()).filter_map(|key| key).collect();
    } else {
      return vec![];
    };

    let mut res = vec![];
    for (o, output) in self.prefix.outputs.iter().enumerate() {
      // TODO: This may be replaceable by pubkeys[o]
      for pubkey in &pubkeys {
        let mut commitment = Commitment::zero();

        // P - shared == spend
        let matches = |shared_key| (output.key - (&shared_key * &ED25519_BASEPOINT_TABLE)) == spend;
        let test = |shared_key| Some(shared_key).filter(|shared_key| matches(*shared_key));

        // Get the traditional shared key and unique shared key, testing if either matches for this output
        let traditional = test(shared_key(None, view, pubkey, o));
        let unique = test(shared_key(Some(uniqueness(&self.prefix.inputs)), view, pubkey, o));

        // If either matches, grab it and decode the amount
        if let Some(key_offset) = traditional.or(unique) {
          // Miner transaction
          if output.amount != 0 {
            commitment.amount = output.amount;
          // Regular transaction
          } else {
            let amount = match self.rct_signatures.base.ecdh_info.get(o) {
              Some(amount) => amount_decryption(*amount, key_offset),
              // This should never happen, yet it may be possible with miner transactions?
              // Using get just decreases the possibility of a panic and lets us move on in that case
              None => continue
            };

            // Rebuild the commitment to verify it
            commitment = Commitment::new(commitment_mask(key_offset), amount);
            // If this is a malicious commitment, move to the next output
            // Any other R value will calculate to a different spend key and are therefore ignorable
            if Some(&commitment.calculate()) != self.rct_signatures.base.commitments.get(o) {
              break;
            }
          }

          if commitment.amount != 0 {
            res.push(SpendableOutput { tx: self.hash(), o, key: output.key, key_offset, commitment });
          }
          // Break to prevent public keys from being included multiple times, triggering multiple
          // inclusions of the same output
          break;
        }
      }
    }
    res
  }
}