use std::collections::HashMap;

use dkg::{Participant, tests::clone_without};

use messages::sign::SignId;

use serai_primitives::{
  BlockHash, crypto::RuntimePublic, PublicKey, SeraiAddress, NetworkId, Coin, Balance,
};
use serai_in_instructions_primitives::{
  InInstruction, InInstructionWithBalance, SignedBatch, batch_message,
};

use dockertest::DockerTest;

use crate::{*, tests::*};

pub(crate) async fn recv_batch_preprocesses(
  coordinators: &mut [Coordinator],
  key: [u8; 32],
  attempt: u32,
) -> (SignId, HashMap<Participant, Vec<u8>>) {
  let mut id = None;
  let mut preprocesses = HashMap::new();
  for (i, coordinator) in coordinators.iter_mut().enumerate() {
    let i = Participant::new(u16::try_from(i).unwrap() + 1).unwrap();

    let msg = coordinator.recv_message().await;
    match msg {
      messages::ProcessorMessage::Coordinator(
        messages::coordinator::ProcessorMessage::BatchPreprocess { id: this_id, preprocess },
      ) => {
        if id.is_none() {
          assert_eq!(&this_id.key, &key);
          assert_eq!(this_id.attempt, attempt);
          id = Some(this_id.clone());
        }
        assert_eq!(&this_id, id.as_ref().unwrap());

        preprocesses.insert(i, preprocess);
      }
      _ => panic!("processor didn't send batch preprocess"),
    }
  }

  // Reduce the preprocesses down to the threshold
  while preprocesses.len() > THRESHOLD {
    preprocesses.remove(
      &Participant::new(
        u16::try_from(OsRng.next_u64() % u64::try_from(COORDINATORS).unwrap()).unwrap() + 1,
      )
      .unwrap(),
    );
  }

  (id.unwrap(), preprocesses)
}

pub(crate) async fn sign_batch(
  coordinators: &mut [Coordinator],
  id: SignId,
  preprocesses: HashMap<Participant, Vec<u8>>,
) -> SignedBatch {
  assert_eq!(preprocesses.len(), THRESHOLD);

  for (i, coordinator) in coordinators.iter_mut().enumerate() {
    let i = Participant::new(u16::try_from(i).unwrap() + 1).unwrap();

    if preprocesses.contains_key(&i) {
      coordinator
        .send_message(messages::coordinator::CoordinatorMessage::BatchPreprocesses {
          id: id.clone(),
          preprocesses: clone_without(&preprocesses, &i),
        })
        .await;
    }
  }

  let mut shares = HashMap::new();
  for (i, coordinator) in coordinators.iter_mut().enumerate() {
    let i = Participant::new(u16::try_from(i).unwrap() + 1).unwrap();

    if preprocesses.contains_key(&i) {
      match coordinator.recv_message().await {
        messages::ProcessorMessage::Coordinator(
          messages::coordinator::ProcessorMessage::BatchShare { id: this_id, share },
        ) => {
          assert_eq!(&this_id, &id);
          shares.insert(i, share);
        }
        _ => panic!("processor didn't send batch share"),
      }
    }
  }

  for (i, coordinator) in coordinators.iter_mut().enumerate() {
    let i = Participant::new(u16::try_from(i).unwrap() + 1).unwrap();

    if preprocesses.contains_key(&i) {
      coordinator
        .send_message(messages::coordinator::CoordinatorMessage::BatchShares {
          id: id.clone(),
          shares: clone_without(&shares, &i),
        })
        .await;
    }
  }

  // The selected processors should yield the batch
  let mut batch = None;
  for (i, coordinator) in coordinators.iter_mut().enumerate() {
    let i = Participant::new(u16::try_from(i).unwrap() + 1).unwrap();

    if preprocesses.contains_key(&i) {
      match coordinator.recv_message().await {
        messages::ProcessorMessage::Substrate(messages::substrate::ProcessorMessage::Update {
          key,
          batch: this_batch,
        }) => {
          assert_eq!(&key, &id.key);

          if batch.is_none() {
            assert!(PublicKey::from_raw(id.key.clone().try_into().unwrap())
              .verify(&batch_message(&this_batch.batch), &this_batch.signature));

            batch = Some(this_batch.clone());
          }

          assert_eq!(batch.as_ref().unwrap(), &this_batch);
        }
        _ => panic!("processor didn't send batch"),
      }
    }
  }
  batch.unwrap()
}

#[test]
fn batch_test() {
  for network in [NetworkId::Bitcoin, NetworkId::Monero] {
    let mut coordinators = vec![];
    let mut test = DockerTest::new();
    for _ in 0 .. COORDINATORS {
      let (handles, coord_key, compositions) = processor_stack(network);
      coordinators.push((handles, coord_key));
      for composition in compositions {
        test.add_composition(composition);
      }
    }

    test.run(|ops| async move {
      tokio::time::sleep(core::time::Duration::from_secs(1)).await;

      let mut coordinators = coordinators
        .into_iter()
        .map(|(handles, key)| Coordinator::new(network, &ops, handles, key))
        .collect::<Vec<_>>();

      // Create a wallet before we start generating keys
      let mut wallet = Wallet::new(network, &ops, coordinators[0].network_handle.clone()).await;
      coordinators[0].sync(&ops, &coordinators[1 ..]).await;

      // Generate keys
      let key_pair = key_gen(&mut coordinators, network).await;

      // Now we we have to mine blocks to activate the key
      // (the first key is activated when the coin's block time exceeds the Serai time it was
      // confirmed at)

      for _ in 0 .. confirmations(network) {
        coordinators[0].add_block(&ops).await;
      }
      coordinators[0].sync(&ops, &coordinators[1 ..]).await;

      // Run twice, once with an instruction and once without
      for i in 0 .. 2 {
        let mut serai_address = [0; 32];
        OsRng.fill_bytes(&mut serai_address);
        let instruction =
          if i == 1 { Some(InInstruction::Transfer(SeraiAddress(serai_address))) } else { None };

        // Send into the processor's wallet
        let (tx, amount_sent) =
          wallet.send_to_address(&ops, &key_pair.1, instruction.clone()).await;
        for coordinator in &mut coordinators {
          coordinator.publish_transacton(&ops, &tx).await;
        }

        // Put the TX past the confirmation depth
        let mut block_with_tx = None;
        for _ in 0 .. confirmations(network) {
          let (hash, _) = coordinators[0].add_block(&ops).await;
          if block_with_tx.is_none() {
            block_with_tx = Some(hash);
          }
        }
        coordinators[0].sync(&ops, &coordinators[1 ..]).await;

        // Sleep for 10s
        // The scanner works on a 5s interval, so this leaves a few s for any processing/latency
        tokio::time::sleep(core::time::Duration::from_secs(10)).await;

        // Make sure the proceessors picked it up by checking they're trying to sign a batch for it
        let (mut id, mut preprocesses) =
          recv_batch_preprocesses(&mut coordinators, key_pair.0 .0, 0).await;
        // Trigger a random amount of re-attempts
        for attempt in 1 ..= u32::try_from(OsRng.next_u64() % 4).unwrap() {
          // TODO: Double check how the processor handles this ID field
          // It should be able to assert its perfectly sequential
          id.attempt = attempt;
          for coordinator in coordinators.iter_mut() {
            coordinator
              .send_message(messages::coordinator::CoordinatorMessage::BatchReattempt {
                id: id.clone(),
              })
              .await;
          }
          (id, preprocesses) =
            recv_batch_preprocesses(&mut coordinators, key_pair.0 .0, attempt).await;
        }

        // Continue with signing the batch
        let batch = sign_batch(&mut coordinators, id, preprocesses).await;

        // Check it
        assert_eq!(batch.batch.network, network);
        assert_eq!(batch.batch.id, i);
        assert_eq!(batch.batch.block, BlockHash(block_with_tx.unwrap()));
        if let Some(instruction) = instruction {
          assert_eq!(
            batch.batch.instructions,
            vec![InInstructionWithBalance {
              instruction,
              balance: Balance {
                coin: match network {
                  NetworkId::Bitcoin => Coin::Bitcoin,
                  NetworkId::Ethereum => todo!(),
                  NetworkId::Monero => Coin::Monero,
                  NetworkId::Serai => panic!("running processor tests on Serai"),
                },
                amount: amount_sent,
              }
            }]
          );
        } else {
          // This shouldn't have an instruction as we didn't add any data into the TX we sent
          // Empty batches remain valuable as they let us achieve consensus on the block and spend
          // contained outputs
          assert!(batch.batch.instructions.is_empty());
        }
      }
    });
  }
}