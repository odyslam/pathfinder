use anyhow::{Context, Error, Result};
use pedersen::{pedersen_hash, StarkHash};
use serde::Serialize;
use sha3::Digest;

use crate::core::ContractHash;

/// Computes the starknet contract hash for given contract definition json blob.
///
/// The structure of the blob is not strictly defined, so it lives in privacy under `json` module
/// of this module. The contract hash has [official documentation][starknet-doc] and [cairo-lang
/// has an implementation][cairo-compute] which is half-python and half-[cairo][cairo-contract].
///
/// Outline of the hashing is:
///
/// 1. contract definition is serialized with python's [`sort_keys=True` option][py-sortkeys], then
///    a truncated Keccak256 hash is calculated of the serialized json
/// 2. a hash chain construction out of [`pedersen_hash`] is used to process in order the contract
///    entry points, builtins, the truncated keccak hash and bytecodes
/// 3. each of the hashchains is hash chained together to produce a final contract hash
///
/// Hash chain construction is explained at the [official documentation][starknet-doc], but it's
/// text explanations are much more complex than the actual implementation in `HashChain`, which
/// you can find from source file of this function.
///
/// [starknet-doc]: https://starknet.io/documentation/contracts/#contract_hash
/// [cairo-compute]: https://github.com/starkware-libs/cairo-lang/blob/64a7f6aed9757d3d8d6c28bd972df73272b0cb0a/src/starkware/starknet/core/os/contract_hash.py
/// [cairo-contract]: https://github.com/starkware-libs/cairo-lang/blob/64a7f6aed9757d3d8d6c28bd972df73272b0cb0a/src/starkware/starknet/core/os/contracts.cairo#L76-L118
/// [py-sortkeys]: https://github.com/starkware-libs/cairo-lang/blob/64a7f6aed9757d3d8d6c28bd972df73272b0cb0a/src/starkware/starknet/core/os/contract_hash.py#L58-L71
pub fn compute_contract_hash(contract_definition_dump: &[u8]) -> Result<ContractHash> {
    let contract_definition =
        serde_json::from_slice::<json::ContractDefinition>(contract_definition_dump)
            .context("Failed to parse contract_definition")?;

    compute_contract_hash0(contract_definition).context("Compute contract hash")
}

/// Sibling functionality to only [`compute_contract_hash`], returning also the ABI, and bytecode
/// parts as json bytes.
pub(crate) fn extract_abi_code_hash(
    contract_definition_dump: &[u8],
) -> Result<(Vec<u8>, Vec<u8>, ContractHash)> {
    let contract_definition =
        serde_json::from_slice::<json::ContractDefinition>(contract_definition_dump)
            .context("Failed to parse contract_definition")?;

    // just in case we'd accidentially modify these in the compute_contract_hash0
    let abi = serde_json::to_vec(&contract_definition.abi)
        .context("Serialize contract_definition.abi")?;
    let code = serde_json::to_vec(&contract_definition.program.data)
        .context("Serialize contract_definition.program.data")?;

    let hash = compute_contract_hash0(contract_definition).context("Compute contract hash")?;

    Ok((abi, code, hash))
}

fn compute_contract_hash0(
    mut contract_definition: json::ContractDefinition<'_>,
) -> Result<ContractHash> {
    use json::EntryPointType::*;

    // the other modification is handled by skipping if the attributes vec is empty
    contract_definition.program.debug_info = None;

    let truncated_keccak = {
        let mut ser =
            serde_json::Serializer::with_formatter(KeccakWriter::default(), PythonDefaultFormatter);

        contract_definition
            .serialize(&mut ser)
            .context("Serializing contract_definition for Keccak256")?;

        let KeccakWriter(hash) = ser.into_inner();
        truncated_keccak(<[u8; 32]>::from(hash.finalize()))
    };

    // what follows is defined over at the contract.cairo

    const API_VERSION: StarkHash = StarkHash::ZERO;

    let mut outer = HashChain::default();

    // This wasn't in the docs, but similarly to contract_state hash, we start with this 0, so this
    // will yield outer == H(0, 0); However, dissimilarly to contract_state hash, we do include the
    // number of items in this contract_hash.
    outer.update(API_VERSION);

    // It is important process the different entrypoint hashchains in correct order.
    // Each of the entrypoint lists gets updated into the `outer` hashchain.
    //
    // This implementation doesn't preparse the strings, which makes it a bit more noisy. Late
    // parsing is made in an attempt to lean on the one big string allocation we've already got,
    // but these three hash chains could be constructed at deserialization time.
    [External, L1Handler, Constructor]
        .iter()
        .map(|key| {
            contract_definition
                .entry_points_by_type
                .get(key)
                .unwrap_or(&Vec::new())
                .iter()
                .enumerate()
                // flatten each entry point to get a list of (selector, offset, selector, offset, ...)
                // `i` is the nth selector of the `key` kind
                .flat_map(|(i, x)| {
                    [("selector", &*x.selector), ("offset", &*x.offset)]
                        .into_iter()
                        .map(move |(field, x)| match x.strip_prefix("0x") {
                            Some(x) => Ok((field, x)),
                            None => Err(anyhow::anyhow!(
                                "Entry point missing '0x' prefix under {key} at index {i} entry ({field})",
                            )),
                        })
                        .map(move |res| {
                            res.and_then(|(field, hex)| {
                                StarkHash::from_hex_str(hex).with_context(|| {
                                    format!("Entry point invalid hex under {key} at index {i} entry ({field})")
                                })
                            })
                        })
                })
                .try_fold(HashChain::default(), |mut hc, next| {
                    hc.update(next?);
                    Result::<_, Error>::Ok(hc)
                })
        })
        .try_for_each(|x| {
            outer.update(x?.finalize());
            Result::<_, Error>::Ok(())
        })
        .context("Failed to process contract_definition.entry_points_by_type")?;

    let builtins = contract_definition
        .program
        .builtins
        .iter()
        .enumerate()
        .map(|(i, s)| (i, s.as_bytes()))
        .map(|(i, s)| {
            StarkHash::from_be_slice(s).with_context(|| format!("Invalid builtin at index {i}"))
        })
        .try_fold(HashChain::default(), |mut hc, next| {
            hc.update(next?);
            Result::<_, Error>::Ok(hc)
        })
        .context("Failed to process contract_definition.program.builtins")?;

    outer.update(builtins.finalize());

    outer.update(truncated_keccak);

    let bytecodes = contract_definition
        .program
        .data
        .iter()
        .enumerate()
        .map(|(i, s)| {
            StarkHash::from_hex_str(&*s).with_context(|| format!("Invalid bytecode at index {i}"))
        })
        .try_fold(HashChain::default(), |mut hc, next| {
            hc.update(next?);
            Result::<_, Error>::Ok(hc)
        })
        .context("Failed to process contract_definition.program.data")?;

    outer.update(bytecodes.finalize());

    Ok(ContractHash(outer.finalize()))
}

/// HashChain is the structure used over at cairo side to represent the hash construction needed
/// for computing the contract hash.
///
/// Empty hash chained value equals `H(0, 0)` where `H` is the [`pedersen_hash`] function, and the
/// second value is the number of values hashed together in this chain. For other values, the
/// accumulator is on each update replaced with the `H(hash, value)` and the number of count
/// incremented by one.
struct HashChain {
    hash: StarkHash,
    count: usize,
}

impl Default for HashChain {
    fn default() -> Self {
        HashChain {
            hash: StarkHash::ZERO,
            count: 0,
        }
    }
}

impl HashChain {
    fn update(&mut self, value: StarkHash) {
        self.hash = pedersen_hash(self.hash, value);
        self.count = self
            .count
            .checked_add(1)
            .expect("could not have deserialized larger than usize Vecs");
    }

    fn finalize(self) -> StarkHash {
        let count = StarkHash::from_be_slice(&self.count.to_be_bytes())
            .expect("usize is smaller than 251-bits");
        pedersen_hash(self.hash, count)
    }
}

/// See:
/// <https://github.com/starkware-libs/cairo-lang/blob/64a7f6aed9757d3d8d6c28bd972df73272b0cb0a/src/starkware/starknet/public/abi.py#L21-L26>
pub(crate) fn truncated_keccak(mut plain: [u8; 32]) -> StarkHash {
    // python code masks with (2**250 - 1) which starts 0x03 and is followed by 31 0xff in be
    // truncation is needed not to overflow the field element.
    plain[0] &= 0x03;
    StarkHash::from_be_bytes(plain).expect("cannot overflow: smaller than modulus")
}

/// `std::io::Write` adapter for Keccak256; we don't need the serialized version in
/// compute_contract_hash, but we need the truncated_keccak hash.
///
/// When debugging mismatching hashes, it might be useful to check the length of each before trying
/// to find the wrongly serialized spot. Example length > 500kB.
#[derive(Default)]
struct KeccakWriter(sha3::Keccak256);

impl std::io::Write for KeccakWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // noop is fine, we'll finalize after the write phase
        Ok(())
    }
}

/// Starkware doesn't use compact formatting for JSON but default python formatting.
/// This is required to hash to the same value after sorted serialization.
struct PythonDefaultFormatter;

impl serde_json::ser::Formatter for PythonDefaultFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        writer.write_all(b": ")
    }
}

mod json {
    use std::borrow::Cow;
    use std::collections::{BTreeMap, HashMap};
    use std::fmt;

    /// Our version of the cairo contract definition used to deserialize and re-serialize a
    /// modified version for a hash of the contract definition.
    ///
    /// The implementation uses `serde_json::Value` extensively for the unknown/undefined
    /// structure, and the correctness of this implementation depends on the following features of
    /// serde_json:
    ///
    /// - feature `raw_value` has to be enabled for the thrown away `program.debug_info`
    /// - feature `preserve_order` has to be disabled, as we want everything sorted
    /// - feature `arbitrary_precision` has to be enabled, as there are big integers in the input
    ///
    /// It would be much more efficient to have a serde_json::Value which would only hold borrowed
    /// types.
    #[derive(serde::Deserialize, serde::Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct ContractDefinition<'a> {
        /// Contract ABI, which has no schema definition.
        pub abi: serde_json::Value,

        /// Main program definition.
        #[serde(borrow)]
        pub program: Program<'a>,

        /// The contract entry points.
        ///
        /// These are left out of the re-serialized version with the ordering requirement to a
        /// Keccak256 hash.
        #[serde(skip_serializing, borrow)]
        pub entry_points_by_type: HashMap<EntryPointType, Vec<SelectorAndOffset<'a>>>,
    }

    #[derive(Copy, Clone, Debug, serde::Deserialize, PartialEq, Hash, Eq)]
    #[serde(deny_unknown_fields)]
    pub enum EntryPointType {
        #[serde(rename = "EXTERNAL")]
        External,
        #[serde(rename = "L1_HANDLER")]
        L1Handler,
        #[serde(rename = "CONSTRUCTOR")]
        Constructor,
    }

    impl fmt::Display for EntryPointType {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            use EntryPointType::*;
            f.pad(match self {
                External => "EXTERNAL",
                L1Handler => "L1_HANDLER",
                Constructor => "CONSTRUCTOR",
            })
        }
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct SelectorAndOffset<'a> {
        #[serde(borrow)]
        pub selector: Cow<'a, str>,
        #[serde(borrow)]
        pub offset: Cow<'a, str>,
    }

    // It's important that this is ordered alphabetically because the fields need to be in
    // sorted order for the keccak hashed representation.
    #[derive(serde::Deserialize, serde::Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Program<'a> {
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        pub attributes: Vec<serde_json::Value>,

        #[serde(borrow)]
        pub builtins: Vec<Cow<'a, str>>,

        #[serde(borrow)]
        pub data: Vec<Cow<'a, str>>,

        #[serde(borrow)]
        pub debug_info: Option<&'a serde_json::value::RawValue>,

        // Important that this is ordered by the numeric keys, not lexicographically
        pub hints: BTreeMap<u64, Vec<serde_json::Value>>,

        pub identifiers: serde_json::Value,

        #[serde(borrow)]
        pub main_scope: Cow<'a, str>,

        // Unlike most other integers, this one is hex string. We don't need to interpret it,
        // it just needs to be part of the hashed output.
        #[serde(borrow)]
        pub prime: Cow<'a, str>,

        pub reference_manager: serde_json::Value,
    }

    #[cfg(test)]
    mod roundtrip_tests {
        // FIXME: we should have many test cases utilizing this.
        #[allow(unused)]
        fn roundtrips<'a, T>(input: &'a str)
        where
            T: serde::Deserialize<'a> + serde::Serialize,
        {
            use super::super::PythonDefaultFormatter;

            let parsed: T = serde_json::from_str(input).unwrap();
            let mut ser =
                serde_json::Serializer::with_formatter(Vec::new(), PythonDefaultFormatter);
            parsed.serialize(&mut ser).unwrap();
            let bytes = ser.into_inner();
            let output = std::str::from_utf8(&bytes).expect("serde does this unchecked");

            // these need to be byte for byte equal because we hash this
            assert_eq!(input, output);
        }
    }

    #[cfg(test)]
    mod test_vectors {
        #[tokio::test]
        async fn first() {
            // this test is a bit on the slow side because of the download and because of the long
            // processing time in dev builds. expected --release speed is 9 contracts/s.
            let expected = pedersen::StarkHash::from_hex_str(
                "0031da92cf5f54bcb81b447e219e2b791b23f3052d12b6c9abd04ff2e5626576",
            )
            .unwrap();

            // this is quite big payload, ~500kB
            let resp = reqwest::get("https://external.integration.starknet.io/feeder_gateway/get_full_contract?contractAddress=0x4ae0618c330c59559a59a27d143dd1c07cd74cf4e5e5a7cd85d53c6bf0e89dc")
                .await
                .unwrap();

            let payload = resp.text().await.expect("response wasn't a string");

            // for bad urls the response looks like:
            // 500
            // {"code": "StarknetErrorCode.UNINITIALIZED_CONTRACT", "message": "Contract with address 2116724861677265616176388745625154424116334641142188761834194304782006389228 is not deployed."}

            let hash = super::super::compute_contract_hash(payload.as_bytes()).unwrap();

            assert_eq!(hash.0, expected);
        }

        #[test]
        fn second() {
            let contract_definition = zstd::decode_all(
                // opening up a file requires a path relative to the test running
                &include_bytes!("../../fixtures/contract_definition.json.zst")[..],
            )
            .unwrap();

            let hash = super::super::compute_contract_hash(&contract_definition).unwrap();

            assert_eq!(
                hash.0,
                pedersen::StarkHash::from_hex_str(
                    "050b2148c0d782914e0b12a1a32abe5e398930b7e914f82c65cb7afce0a0ab9b"
                )
                .unwrap()
            );
        }

        #[tokio::test]
        async fn genesis_contract() {
            use pedersen::StarkHash;
            let contract = StarkHash::from_hex_str(
                "0x0546BA9763D33DC59A070C0D87D94F2DCAFA82C4A93B5E2BF5AE458B0013A9D3",
            )
            .unwrap();
            let contract = crate::core::ContractAddress(contract);

            let chain = crate::ethereum::Chain::Goerli;
            let sequencer = crate::sequencer::Client::new(chain).unwrap();
            let contract_definition = sequencer
                .full_contract(contract)
                .await
                .expect("Download contract from sequencer");

            let _ = crate::state::contract_hash::compute_contract_hash(&contract_definition)
                .expect("Extract and compute  hash");
        }
    }

    #[cfg(test)]
    mod test_serde_features {
        #[test]
        fn serde_json_value_sorts_maps() {
            // this property is leaned on and the default implementation of serde_json works like
            // this. serde_json has a feature called "preserve_order" which could get enabled by
            // accident, and it would destroy the ability to compute_contract_hash.

            let input = r#"{"foo": 1, "bar": 2}"#;
            let parsed = serde_json::from_str::<serde_json::Value>(input).unwrap();
            let output = serde_json::to_string(&parsed).unwrap();

            assert_eq!(output, r#"{"bar":2,"foo":1}"#);
        }

        #[test]
        fn serde_json_has_arbitrary_precision() {
            // the json has 251-bit ints, python handles them out of box, serde_json requires
            // feature "arbitrary_precision".

            // this is 2**256 - 1
            let input = r#"{"foo":115792089237316195423570985008687907853269984665640564039457584007913129639935}"#;

            let output =
                serde_json::to_string(&serde_json::from_str::<serde_json::Value>(input).unwrap())
                    .unwrap();

            assert_eq!(input, output);
        }

        #[test]
        fn serde_json_has_raw_value() {
            // raw value is needed for others but here for completness; this shouldn't compile if
            // you the feature wasn't enabled.

            #[derive(serde::Deserialize, serde::Serialize)]
            struct Program<'a> {
                #[serde(borrow)]
                debug_info: Option<&'a serde_json::value::RawValue>,
            }

            let mut input = serde_json::from_str::<Program>(
                r#"{"debug_info": {"long": {"tree": { "which": ["we dont", "care", "about", 0] }}}}"#,
            ).unwrap();

            input.debug_info = None;

            let output = serde_json::to_string(&input).unwrap();

            assert_eq!(output, r#"{"debug_info":null}"#);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn truncated_keccak_matches_pythonic() {
        use super::truncated_keccak;
        use pedersen::StarkHash;
        use sha3::{Digest, Keccak256};
        let all_set = Keccak256::digest(&[0xffu8; 32]);
        assert!(all_set[0] > 0xf);
        let truncated = truncated_keccak(all_set.into());
        assert_eq!(
            truncated,
            StarkHash::from_hex_str(
                "01c584056064687e149968cbab758a3376d22aedc6a55823d1b3ecbee81b8fb9"
            )
            .unwrap()
        );
    }
}
