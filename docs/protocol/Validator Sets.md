# Validator Sets

Validator Sets are defined at the protocol level, with the following parameters:

- `index` (VS):        Validator set index, a global key atomically increasing
from 0.
- `bond`  (Amount):    Amount of bond per key-share of this validator set.
- `coins` (Vec<Coin>): Coins managed by this validator set.

At launch, there will solely be validator set 0, managing Bitcoin, Ethereum,
USDC, DAI, and Monero.

### Multisig Management

Every validator set is expected to form a multisig per curve required by its
coins. This multisig is secure to hold funds up to 67% of the validator set's
bond value.

### Participation in the BFT process

All validator sets participate in the BFT process. Specifically, a block
containing `Oraclization`s for a coin must be approved by the BFT majority of
the validator set responsible for it, along with the BFT majority of the network
by bond.

At this time, `Oraclization`s for a coin are only expected to be included when a
validator from the validator set managing the coin is the producer of the block
in question.