use snow::Builder;

use crate::keys::StaticKeypair;

const PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"weft v1";
pub(crate) const MAX_HANDSHAKE_LEN: usize = 512;
pub(crate) const MAX_HANDSHAKE_PADDING: usize = 200;

pub(crate) fn builder(keypair: &StaticKeypair) -> Builder<'_> {
    Builder::new(PARAMS.parse().expect("valid noise params"))
        .local_private_key(keypair.secret())
        .and_then(|builder| builder.prologue(PROLOGUE))
        .expect("valid noise configuration")
}
