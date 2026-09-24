//! SPEC §13.3: private credential provisioning is separate from command evidence.
use crate::{identity_storage::IdentityDirectory, ingress::IngressScope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
#[derive(Debug, thiserror::Error)]
#[error("private ingress identity unavailable")]
pub struct IngressIdentityError;
/// No Debug/Serialize: raw native credentials stay in the protected host bundle.
pub struct NativeCredentials {
    pub gate: [u8; 32],
    pub inference: [u8; 32],
    pub admin: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    version: u32,
    scope: IngressScope,
    command_digest: [u8;32],
    gate: [u8; 32],
    inference: [u8; 32],
    admin: [u8; 32],
}
fn name(scope: &IngressScope) -> Result<String, IngressIdentityError> {
    if scope.generation <= 0
        || scope.revision <= 0
        || [
            &scope.host_id,
            &scope.deployment_id,
            &scope.binding_id,
            &scope.incarnation,
            &scope.member_id,
        ]
        .iter()
        .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
    {
        return Err(IngressIdentityError);
    }
    Ok(format!(
        "ingress-{:x}.json",
        Sha256::digest(scope.binding_id.as_bytes())
    ))
}
fn key() -> Result<[u8; 32], IngressIdentityError> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| IngressIdentityError)?;
    if bytes == [0; 32] {
        return Err(IngressIdentityError);
    }
    Ok(bytes)
}
fn decode(bytes: &[u8], scope: &IngressScope, command_digest:[u8;32]) -> Result<NativeCredentials, IngressIdentityError> {
    let bundle: Bundle = serde_json::from_slice(bytes).map_err(|_| IngressIdentityError)?;
    // ADR 0013 §5: a bundle written before the ingress was keyed by instance
    // names none. Its file is named by the binding, which realizes exactly one
    // instance, so it stays valid for the scope that differs only there.
    let legacy = serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .is_some_and(|value| value["scope"].get("instance_index").is_none());
    let same_scope = bundle.scope == *scope
        || (legacy
            && bundle.scope
                == IngressScope {
                    instance_index: 0,
                    ..scope.clone()
                });
    if bundle.version != 1
        || !same_scope
        || bundle.command_digest != command_digest
        || command_digest == [0;32]
        || bundle.gate == [0; 32]
        || bundle.inference == [0; 32]
        || bundle.admin == [0; 32]
        || bundle.gate == bundle.inference
        || bundle.gate == bundle.admin
        || bundle.inference == bundle.admin
    {
        return Err(IngressIdentityError);
    }
    Ok(NativeCredentials {
        gate: bundle.gate,
        inference: bundle.inference,
        admin: bundle.admin,
    })
}
pub fn load(
    storage: &IdentityDirectory,
    scope: &IngressScope,
    command_digest:[u8;32],
) -> Result<NativeCredentials, IngressIdentityError> {
    let bytes = storage
        .read_bundle(&name(scope)?)
        .map_err(|_| IngressIdentityError)?
        .ok_or(IngressIdentityError)?;
    decode(&bytes, scope,command_digest)
}
pub fn provision(
    storage: &IdentityDirectory,
    scope: &IngressScope,
    command_digest:[u8;32],
    gate: [u8; 32],
) -> Result<NativeCredentials, IngressIdentityError> {
    let filename = name(scope)?;
    if gate == [0; 32] {
        return Err(IngressIdentityError);
    }
    if let Some(bytes) = storage
        .read_bundle(&filename)
        .map_err(|_| IngressIdentityError)?
    {
        let existing = decode(&bytes, scope,command_digest)?;
        return if existing.gate == gate {
            Ok(existing)
        } else {
            Err(IngressIdentityError)
        };
    }
    let bundle = Bundle {
        version: 1,
        scope: scope.clone(),
        command_digest,
        gate,
        inference: key()?,
        admin: key()?,
    };
    let bytes = serde_json::to_vec(&bundle).map_err(|_| IngressIdentityError)?;
    let credentials = decode(&bytes, scope,command_digest)?;
    storage
        .create_bundle(&filename, &bytes)
        .map_err(|_| IngressIdentityError)?;
    Ok(credentials)
}

pub struct IngressIdentities {storage:IdentityDirectory}
impl IngressIdentities {
    pub fn new(storage:IdentityDirectory)->std::sync::Arc<Self> {std::sync::Arc::new(Self {storage})}
    pub fn provision(&self,scope:&IngressScope,command_digest:[u8;32],gate:[u8;32])->Result<NativeCredentials,IngressIdentityError> {
        provision(&self.storage,scope,command_digest,gate)
    }
    pub fn load(&self,scope:&IngressScope,command_digest:[u8;32])->Result<NativeCredentials,IngressIdentityError> {
        load(&self.storage,scope,command_digest)
    }
    /// SPEC §13.3 / T37: delete the credentials of exactly this launch (scope
    /// and command digest) once it is settled: proven gone, or refused before
    /// any effect. A bundle naming another launch is left alone.
    pub fn retire(&self,scope:&IngressScope,command_digest:[u8;32])->Result<(),IngressIdentityError> {
        let filename = name(scope)?;
        let Some(bytes) = self.storage.read_bundle(&filename).map_err(|_| IngressIdentityError)? else {
            return Ok(());
        };
        decode(&bytes, scope, command_digest)?;
        self.storage.remove_bundle(&filename).map_err(|_| IngressIdentityError)
    }
}
