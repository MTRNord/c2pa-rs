// Copyright 2022 Adobe. All rights reserved.
// This file is licensed to you under the Apache License,
// Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0)
// or the MIT license (http://opensource.org/licenses/MIT),
// at your option.

// Unless required by applicable law or agreed to in writing,
// this software is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR REPRESENTATIONS OF ANY KIND, either express or
// implied. See the LICENSE-MIT and LICENSE-APACHE files for the
// specific language governing permissions and limitations under
// each license.

use async_generic::async_generic;
use bcder::encode::Values;
use bcder::{decode::Constructed, ConstOid};
use coset::{sig_structure_data, ProtectedHeader};
use serde::{Deserialize, Serialize};
use x509_certificate::DigestAlgorithm::{self};

use crate::{
    asn1::{
        rfc3161::{TimeStampResp, TstInfo, OID_CONTENT_TYPE_TST_INFO},
        rfc5652::{
            CertificateChoices::Certificate, SignedAttributesDer, SignedData, OID_ID_SIGNED_DATA,
        },
    },
    cose_validator::{
        ECDSA_WITH_SHA256_OID, ECDSA_WITH_SHA384_OID, ECDSA_WITH_SHA512_OID, EC_PUBLICKEY_OID,
        ED25519_OID, RSA_OID, SHA1_OID, SHA256_OID, SHA256_WITH_RSAENCRYPTION_OID, SHA384_OID,
        SHA384_WITH_RSAENCRYPTION_OID, SHA512_OID, SHA512_WITH_RSAENCRYPTION_OID,
    },
    hash_utils::vec_compare,
    AsyncSigner, Signer,
};
/// Generate TimeStamp signature according to https://datatracker.ietf.org/doc/html/rfc3161
/// using the specified Time Authority
use crate::{
    //cose_validator::{SHA256_OID, SHA384_OID, SHA512_OID},
    error::{Error, Result},
};

#[allow(dead_code)]
pub(crate) fn cose_countersign_data(data: &[u8], p_header: &ProtectedHeader) -> Vec<u8> {
    let aad: Vec<u8> = Vec::new();

    // create sig_structure_data to be signed
    sig_structure_data(
        coset::SignatureContext::CounterSignature,
        p_header.clone(),
        None,
        &aad,
        data,
    )
}

#[async_generic(
    async_signature(
        signer: &dyn AsyncSigner,
        data: &[u8],
        p_header: &ProtectedHeader,
    ))]
pub(crate) fn cose_timestamp_countersign(
    signer: &dyn Signer,
    data: &[u8],
    p_header: &ProtectedHeader,
) -> Option<Result<Vec<u8>>> {
    // create countersignature with TimeStampReq parameters
    // payload: data
    // context "CounterSigner"
    // certReq true
    // algorithm sha256

    // create sig data structure to be time stamped
    let sd = cose_countersign_data(data, p_header);

    if _sync {
        timestamp_data(signer, &sd)
    } else {
        timestamp_data_async(signer, &sd).await
    }
}

#[async_generic]
pub(crate) fn cose_sigtst_to_tstinfos(
    sigtst_cbor: &[u8],
    data: &[u8],
    p_header: &ProtectedHeader,
) -> Result<Vec<TstInfo>> {
    let tst_container: TstContainer =
        serde_cbor::from_slice(sigtst_cbor).map_err(|_err| Error::CoseTimeStampGeneration)?;

    let mut tstinfos: Vec<TstInfo> = Vec::new();

    for token in &tst_container.tst_tokens {
        let tbs = cose_countersign_data(data, p_header);
        let tst_info = if _sync {
            verify_timestamp(&token.val, &tbs)?
        } else {
            verify_timestamp_async(&token.val, &tbs).await?
        };

        tstinfos.push(tst_info);
    }

    if tstinfos.is_empty() {
        Err(Error::NotFound)
    } else {
        Ok(tstinfos)
    }
}

/// internal only function to work around bug in serialization of TimeStampResponse
/// so we just return the data directly
#[cfg(not(target_arch = "wasm32"))]
fn time_stamp_request_http(
    url: &str,
    headers: Option<Vec<(String, String)>>,
    request: &crate::asn1::rfc3161::TimeStampReq,
) -> Result<Vec<u8>> {
    use std::io::Read;

    const HTTP_CONTENT_TYPE_REQUEST: &str = "application/timestamp-query";
    const HTTP_CONTENT_TYPE_RESPONSE: &str = "application/timestamp-reply";

    let mut body = Vec::<u8>::new();
    request
        .encode_ref()
        .write_encoded(bcder::Mode::Der, &mut body)?;

    let body_reader = std::io::Cursor::new(body);

    let mut req = ureq::post(url);

    if let Some(headers) = headers {
        for (ref name, ref value) in headers {
            req = req.set(name.as_str(), value.as_str());
        }
    }

    let response = req
        .set("Content-Type", HTTP_CONTENT_TYPE_REQUEST)
        .send(body_reader)
        .map_err(|_err| Error::CoseTimeStampGeneration)?;

    if response.status() == 200 && response.content_type() == HTTP_CONTENT_TYPE_RESPONSE {
        let len = response
            .header("Content-Length")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(20000);

        let mut response_bytes: Vec<u8> = Vec::with_capacity(len);

        response
            .into_reader()
            .take(1000000)
            .read_to_end(&mut response_bytes)
            .map_err(|_err| Error::CoseTimeStampGeneration)?;

        let res = TimeStampResponse(
            Constructed::decode(response_bytes.as_ref(), bcder::Mode::Der, |cons| {
                TimeStampResp::take_from(cons)
            })
            .map_err(|_err| Error::CoseTimeStampGeneration)?,
        );

        // Verify nonce was reflected, if present.
        if res.is_success() {
            if let Some(tst_info) = res
                .tst_info()
                .map_err(|_err| Error::CoseTimeStampGeneration)?
            {
                if tst_info.nonce != request.nonce {
                    return Err(Error::CoseTimeStampGeneration);
                }
            }
        }

        Ok(response_bytes)
    } else {
        Err(Error::CoseTimeStampGeneration)
    }
}

/// Send a Time-Stamp request for a given message to an HTTP URL.
///
/// This is a wrapper around [time_stamp_request_http] that constructs the low-level
/// ASN.1 request object with reasonable defaults.

pub(crate) fn time_stamp_message_http(
    message: &[u8],
    digest_algorithm: DigestAlgorithm,
) -> Result<crate::asn1::rfc3161::TimeStampReq> {
    use rand::{thread_rng, Rng};

    let mut h = digest_algorithm.digester();
    h.update(message);
    let digest = h.finish();

    let mut random = [0u8; 8];
    thread_rng()
        .try_fill(&mut random)
        .map_err(|_| Error::CoseTimeStampGeneration)?;

    let request = crate::asn1::rfc3161::TimeStampReq {
        version: bcder::Integer::from(1_u8),
        message_imprint: crate::asn1::rfc3161::MessageImprint {
            hash_algorithm: digest_algorithm.into(),
            hashed_message: bcder::OctetString::new(bytes::Bytes::copy_from_slice(digest.as_ref())),
        },
        req_policy: None,
        nonce: Some(bcder::Integer::from(u64::from_le_bytes(random))),
        cert_req: Some(true),
        extensions: None,
    };

    Ok(request)
}

pub struct TimeStampResponse(TimeStampResp);

impl std::ops::Deref for TimeStampResponse {
    type Target = TimeStampResp;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TimeStampResponse {
    /// Whether the time stamp request was successful.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn is_success(&self) -> bool {
        matches!(
            self.0.status.status,
            crate::asn1::rfc3161::PkiStatus::Granted
                | crate::asn1::rfc3161::PkiStatus::GrantedWithMods
        )
    }

    fn signed_data(&self) -> Result<Option<SignedData>> {
        if let Some(token) = &self.0.time_stamp_token {
            if token.content_type == OID_ID_SIGNED_DATA {
                Ok(Some(
                    token
                        .content
                        .clone()
                        .decode(SignedData::take_from)
                        .map_err(|_err| Error::CoseTimeStampGeneration)?,
                ))
            } else {
                Err(Error::CoseTimeStampGeneration)
            }
        } else {
            Ok(None)
        }
    }

    fn tst_info(&self) -> Result<Option<TstInfo>> {
        if let Some(signed_data) = self.signed_data()? {
            if signed_data.content_info.content_type == OID_CONTENT_TYPE_TST_INFO {
                if let Some(content) = signed_data.content_info.content {
                    Ok(Some(
                        Constructed::decode(content.to_bytes(), bcder::Mode::Der, |cons| {
                            TstInfo::take_from(cons)
                        })
                        .map_err(|_err| Error::CoseTimeStampGeneration)?,
                    ))
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }
}

/// Generate TimeStamp based on rfc3161 using "data" as MessageImprint and return raw TimeStampRsp bytes
#[async_generic(async_signature(signer: &dyn AsyncSigner, data: &[u8]))]
pub fn timestamp_data(signer: &dyn Signer, data: &[u8]) -> Option<Result<Vec<u8>>> {
    if _sync {
        signer.send_timestamp_request(data)
    } else {
        signer.send_timestamp_request(data).await
        // TO DO: Fix bug in async_generic. This .await
        // should be automatically removed.
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn default_rfc3161_request(
    url: &str,
    headers: Option<Vec<(String, String)>>,
    data: &[u8],
    message: &[u8],
) -> Result<Vec<u8>> {
    use crate::asn1::rfc3161::TimeStampReq;
    let request = Constructed::decode(
        bcder::decode::SliceSource::new(data),
        bcder::Mode::Der,
        TimeStampReq::take_from,
    )
    .map_err(|_err| Error::CoseTimeStampGeneration)?;

    let ts = time_stamp_request_http(url, headers, &request)?;

    // sanity check
    verify_timestamp(&ts, message)?;

    Ok(ts)
}

#[allow(unused_variables)]
pub fn default_rfc3161_message(data: &[u8]) -> Result<Vec<u8>> {
    let request = time_stamp_message_http(data, x509_certificate::DigestAlgorithm::Sha256)?;

    let mut body = Vec::<u8>::new();
    request
        .encode_ref()
        .write_encoded(bcder::Mode::Der, &mut body)?;
    Ok(body)
}

pub fn gt_to_datetime(
    gt: x509_certificate::asn1time::GeneralizedTime,
) -> chrono::DateTime<chrono::Utc> {
    gt.into()
}
fn time_to_datetime(t: x509_certificate::asn1time::Time) -> chrono::DateTime<chrono::Utc> {
    match t {
        x509_certificate::asn1time::Time::UtcTime(u) => *u,
        x509_certificate::asn1time::Time::GeneralTime(gt) => gt_to_datetime(gt),
    }
}

#[cfg(feature = "openssl")]
fn get_local_validator(
    sig_alg: &bcder::Oid,
    hash_alg: &bcder::Oid,
    pk_alg: &bcder::Oid,
) -> Result<Box<dyn crate::validator::CoseValidator>> {
    let validator = if sig_alg.as_ref() == RSA_OID.as_bytes()
        || pk_alg.as_ref() == SHA256_WITH_RSAENCRYPTION_OID.as_bytes()
        || pk_alg.as_ref() == SHA384_WITH_RSAENCRYPTION_OID.as_bytes()
        || pk_alg.as_ref() == SHA512_WITH_RSAENCRYPTION_OID.as_bytes()
    {
        if hash_alg.as_ref() == SHA1_OID.as_bytes() {
            Box::new(crate::openssl::RsaLegacyValidator::new("sha1"))
        } else if hash_alg.as_ref() == SHA256_OID.as_bytes() {
            Box::new(crate::openssl::RsaLegacyValidator::new("rs256"))
        } else if hash_alg.as_ref() == SHA384_OID.as_bytes() {
            Box::new(crate::openssl::RsaLegacyValidator::new("rs384"))
        } else if hash_alg.as_ref() == SHA512_OID.as_bytes() {
            Box::new(crate::openssl::RsaLegacyValidator::new("rs512"))
        } else {
            return Err(Error::CoseTimeStampAuthority);
        }
    } else if sig_alg.as_ref() == EC_PUBLICKEY_OID.as_bytes()
        || pk_alg.as_ref() == ECDSA_WITH_SHA256_OID.as_bytes()
        || pk_alg.as_ref() == ECDSA_WITH_SHA384_OID.as_bytes()
        || pk_alg.as_ref() == ECDSA_WITH_SHA512_OID.as_bytes()
    {
        if hash_alg.as_ref() == SHA256_OID.as_bytes() {
            crate::validator::get_validator(crate::SigningAlg::Es256)
        } else if hash_alg.as_ref() == SHA384_OID.as_bytes() {
            crate::validator::get_validator(crate::SigningAlg::Es384)
        } else if hash_alg.as_ref() == SHA512_OID.as_bytes() {
            crate::validator::get_validator(crate::SigningAlg::Es512)
        } else {
            return Err(Error::CoseTimeStampAuthority);
        }
    } else if sig_alg.as_ref() == ED25519_OID.as_bytes() {
        crate::validator::get_validator(crate::SigningAlg::Ed25519)
    } else {
        return Err(Error::CoseTimeStampAuthority);
    };

    Ok(validator)
}

#[cfg(target_arch = "wasm32")]
fn get_validator_type(
    sig_alg: &bcder::Oid,
    hash_alg: &bcder::Oid,
    pk_alg: &bcder::Oid,
) -> Option<String> {
    if sig_alg.as_ref() == RSA_OID.as_bytes()
        || pk_alg.as_ref() == SHA256_WITH_RSAENCRYPTION_OID.as_bytes()
        || pk_alg.as_ref() == SHA384_WITH_RSAENCRYPTION_OID.as_bytes()
        || pk_alg.as_ref() == SHA512_WITH_RSAENCRYPTION_OID.as_bytes()
    {
        if hash_alg.as_ref() == SHA1_OID.as_bytes() {
            Some("sha1".to_string())
        } else if hash_alg.as_ref() == SHA256_OID.as_bytes() {
            Some("rs256".to_string())
        } else if hash_alg.as_ref() == SHA384_OID.as_bytes() {
            Some("rs384".to_string())
        } else if hash_alg.as_ref() == SHA512_OID.as_bytes() {
            Some("rs512".to_string())
        } else {
            None
        }
    } else if sig_alg.as_ref() == EC_PUBLICKEY_OID.as_bytes()
        || pk_alg.as_ref() == ECDSA_WITH_SHA256_OID.as_bytes()
        || pk_alg.as_ref() == ECDSA_WITH_SHA384_OID.as_bytes()
        || pk_alg.as_ref() == ECDSA_WITH_SHA512_OID.as_bytes()
    {
        if hash_alg.as_ref() == SHA256_OID.as_bytes() {
            Some(crate::SigningAlg::Es256.to_string())
        } else if hash_alg.as_ref() == SHA384_OID.as_bytes() {
            Some(crate::SigningAlg::Es384.to_string())
        } else if hash_alg.as_ref() == SHA512_OID.as_bytes() {
            Some(crate::SigningAlg::Es512.to_string())
        } else {
            None
        }
    } else if sig_alg.as_ref() == ED25519_OID.as_bytes() {
        Some(crate::SigningAlg::Ed25519.to_string())
    } else {
        None
    }
}

/// Returns TimeStamp token info if ts verifies against supplied data
#[async_generic]
pub fn verify_timestamp(ts: &[u8], data: &[u8]) -> Result<TstInfo> {
    let ts_resp = get_timestamp_response(ts)?;

    // check for timestamp expiration during stamping
    if let Ok(Some(sd)) = &ts_resp.signed_data() {
        let certs = sd
            .certificates
            .clone()
            .ok_or(Error::CoseTimeStampValidity)?;

        for signer_info in sd.signer_infos.iter() {
            if let Some(cert) = certs.iter().find_map(|cc| {
                let c = match cc {
                    Certificate(c) => c,
                    _ => return None,
                };

                match &signer_info.sid {
                    crate::asn1::rfc5652::SignerIdentifier::IssuerAndSerialNumber(sn) => {
                        if sn.issuer == c.tbs_certificate.issuer
                            && sn.serial_number == c.tbs_certificate.serial_number
                        {
                            Some(c)
                        } else {
                            None
                        }
                    }
                    crate::asn1::rfc5652::SignerIdentifier::SubjectKeyIdentifier(ski) => {
                        const SKI_OID: ConstOid = bcder::Oid(&[2, 5, 29, 14]);
                        if let Some(extensions) = &c.tbs_certificate.extensions {
                            if extensions.iter().any(|e| {
                                if e.id == SKI_OID {
                                    return *ski == e.value;
                                }
                                false
                            }) {
                                Some(c)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                }
            }) {
                // timestamp cert expiration
                let tst_opt = ts_resp.tst_info()?;
                let tst = tst_opt.ok_or(Error::CoseInvalidTimeStamp)?;

                let signing_time = gt_to_datetime(tst.gen_time.clone()).timestamp();
                let not_before =
                    time_to_datetime(cert.tbs_certificate.validity.not_before.clone()).timestamp();

                let not_after =
                    time_to_datetime(cert.tbs_certificate.validity.not_after.clone()).timestamp();

                if !(signing_time >= not_before && signing_time <= not_after) {
                    return Err(Error::CoseTimeStampValidity);
                }

                // build CMS structure to verify
                #[allow(unused_variables)]
                let tbs = match &signer_info.signed_attributes {
                    Some(d) => {
                        let sa_der = SignedAttributesDer::new(d.clone(), None);

                        let mut tbs_der = Vec::new();
                        sa_der.write_encoded(bcder::Mode::Der, &mut tbs_der)?;

                        tbs_der
                    }
                    None => match &sd.content_info.content {
                        Some(d) => d.to_bytes().to_vec(),
                        None => return Err(Error::CoseTimeStampValidity),
                    },
                };

                #[allow(unused_variables)]
                let sig_val = &signer_info.signature;
                #[allow(unused_variables)]
                let mut signing_key_der = Vec::<u8>::new();
                cert.tbs_certificate
                    .subject_public_key_info
                    .encode_ref()
                    .write_encoded(bcder::Mode::Der, &mut signing_key_der)?;

                // verify signature of timestamp signature

                #[allow(unused_variables)]
                let hash_alg = &signer_info.digest_algorithm.algorithm;
                #[allow(unused_variables)]
                let sig_alg = &signer_info.signature_algorithm.algorithm;
                #[allow(unused_variables)]
                let pk_alg = &cert.tbs_certificate.signature.algorithm;

                let validated = if _sync {
                    #[cfg(feature = "openssl")]
                    {
                        let validator = get_local_validator(sig_alg, hash_alg, pk_alg)?;
                        validator.validate(&sig_val.to_bytes(), &tbs, &signing_key_der)?
                    }

                    #[cfg(not(feature = "openssl"))]
                    {
                        false
                    }
                } else {
                    #[cfg(feature = "openssl")]
                    {
                        let validator = get_local_validator(sig_alg, hash_alg, pk_alg)?;
                        validator.validate(&sig_val.to_bytes(), &tbs, &signing_key_der)?
                    }

                    #[cfg(target_arch = "wasm32")]
                    {
                        crate::wasm::verify_data(
                            signing_key_der,
                            get_validator_type(sig_alg, hash_alg, pk_alg),
                            sig_val.to_bytes().to_vec(),
                            tbs,
                        )
                        .await?
                    }

                    #[cfg(all(not(feature = "openssl"), not(target_arch = "wasm32")))]
                    {
                        false
                    }
                };

                if validated {
                    // make sure this signature matches the expected data
                    let tst_opt = ts_resp.tst_info()?;
                    let tst = tst_opt.ok_or(Error::CoseInvalidTimeStamp)?;
                    let mi = &tst.message_imprint;

                    let digest_algorithm = DigestAlgorithm::try_from(&mi.hash_algorithm.algorithm)
                        .map_err(|_e| Error::UnsupportedType)?;

                    let mut h = digest_algorithm.digester();
                    h.update(data);
                    let digest = h.finish();

                    if !vec_compare(digest.as_ref(), &mi.hashed_message.to_bytes()) {
                        return Err(Error::CoseTimeStampMismatch);
                    }
                    return Ok(tst);
                } else {
                    return Err(Error::CoseTimeStampAuthority);
                }
            }
        }
    }
    Err(Error::CoseInvalidTimeStamp)
}

/// Get TimeStampResponse from DER TimeStampResp bytes
pub fn get_timestamp_response(tsresp: &[u8]) -> Result<TimeStampResponse> {
    let ts = TimeStampResponse(
        Constructed::decode(tsresp, bcder::Mode::Der, |cons| {
            TimeStampResp::take_from(cons)
        })
        .map_err(|_e| Error::CoseInvalidTimeStamp)?,
    );

    Ok(ts)
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Clone)]
pub struct TstToken {
    #[serde(with = "serde_bytes")]
    pub val: Vec<u8>,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Clone)]
pub struct TstContainer {
    #[serde(rename = "tstTokens")]
    pub tst_tokens: Vec<TstToken>,
}

impl TstContainer {
    pub fn new() -> Self {
        TstContainer {
            tst_tokens: Vec::new(),
        }
    }

    pub fn add_token(&mut self, token: TstToken) {
        self.tst_tokens.push(token);
    }
}

impl Default for TstContainer {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap rfc3161 TimeStampRsp in COSE sigTst object
pub fn make_cose_timestamp(ts_data: &[u8]) -> TstContainer {
    let token = TstToken {
        val: ts_data.to_vec(),
    };

    let mut container = TstContainer::new();
    container.add_token(token);

    container
}
