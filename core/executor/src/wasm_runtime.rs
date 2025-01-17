// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Traits and accessor functions for calling into the Substrate Wasm runtime.
//!
//! The primary means of accessing the runtimes is through a cache which saves the reusable
//! components of the runtime that are expensive to initialize.

use crate::error::{Error, WasmError};
use crate::wasmi_execution;
#[cfg(feature = "wasmtime")]
use crate::wasmtime;
use log::{trace, warn};
use codec::Decode;
use primitives::{storage::well_known_keys, traits::Externalities, H256};
use runtime_version::RuntimeVersion;
use std::{collections::hash_map::{Entry, HashMap}, panic::AssertUnwindSafe};

/// The Substrate Wasm runtime.
pub trait WasmRuntime {
	/// Attempt to update the number of heap pages available during execution.
	///
	/// Returns false if the update cannot be applied. The function is guaranteed to return true if
	/// the heap pages would not change from its current value.
	fn update_heap_pages(&mut self, heap_pages: u64) -> bool;

	/// Call a method in the Substrate runtime by name. Returns the encoded result on success.
	fn call(&mut self, ext: &mut dyn Externalities, method: &str, data: &[u8])
		-> Result<Vec<u8>, Error>;
}

/// Specification of different methods of executing the runtime Wasm code.
#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
pub enum WasmExecutionMethod {
	/// Uses the Wasmi interpreter.
	Interpreted,
	/// Uses the Wasmtime compiled runtime.
	#[cfg(feature = "wasmtime")]
	Compiled,
}

/// A Wasm runtime object along with its cached runtime version.
struct VersionedRuntime {
	runtime: Box<dyn WasmRuntime>,
	/// Runtime version according to `Core_version`.
	version: RuntimeVersion,
}

/// Cache for the runtimes.
///
/// When an instance is requested for the first time it is added to this cache. Metadata is kept
/// with the instance so that it can be efficiently reinitialized.
///
/// When using the Wasmi interpreter execution method, the metadata includes the initial memory and
/// values of mutable globals. Follow-up requests to fetch a runtime return this one instance with
/// the memory reset to the initial memory. So, one runtime instance is reused for every fetch
/// request.
///
/// For now the cache grows indefinitely, but that should be fine for now since runtimes can only be
/// upgraded rarely and there are no other ways to make the node to execute some other runtime.
pub struct RuntimesCache {
	/// A cache of runtime instances along with metadata, ready to be reused.
	///
	/// Instances are keyed by the Wasm execution method and the hash of their code.
	instances: HashMap<(WasmExecutionMethod, [u8; 32]), Result<VersionedRuntime, WasmError>>,
}

impl RuntimesCache {
	/// Creates a new instance of a runtimes cache.
	pub fn new() -> RuntimesCache {
		RuntimesCache {
			instances: HashMap::new(),
		}
	}

	/// Fetches an instance of the runtime.
	///
	/// On first use we create a new runtime instance, save it to the cache
	/// and persist its initial memory.
	///
	/// Each subsequent request will return this instance, with its memory restored
	/// to the persisted initial memory. Thus, we reuse one single runtime instance
	/// for every `fetch_runtime` invocation.
	///
	/// # Parameters
	///
	/// `ext` - Externalities to use for the runtime. This is used for setting
	/// up an initial runtime instance.
	///
	/// `default_heap_pages` - Number of 64KB pages to allocate for Wasm execution.
	///
	/// # Return value
	///
	/// If no error occurred a tuple `(&mut WasmRuntime, H256)` is
	/// returned. `H256` is the hash of the runtime code.
	///
	/// In case of failure one of two errors can be returned:
	///
	/// `Err::InvalidCode` is returned for runtime code issues.
	///
	/// `Error::InvalidMemoryReference` is returned if no memory export with the
	/// identifier `memory` can be found in the runtime.
	pub fn fetch_runtime<E: Externalities>(
		&mut self,
		ext: &mut E,
		wasm_method: WasmExecutionMethod,
		default_heap_pages: u64,
	) -> Result<(&mut (dyn WasmRuntime + 'static), &RuntimeVersion, H256), Error> {
		let code_hash = ext
			.original_storage_hash(well_known_keys::CODE)
			.ok_or(Error::InvalidCode("`CODE` not found in storage.".into()))?;

		let heap_pages = ext
			.storage(well_known_keys::HEAP_PAGES)
			.and_then(|pages| u64::decode(&mut &pages[..]).ok())
			.unwrap_or(default_heap_pages);

		let result = match self.instances.entry((wasm_method, code_hash.into())) {
			Entry::Occupied(o) => {
				let result = o.into_mut();
				if let Ok(ref mut cached_runtime) = result {
					if !cached_runtime.runtime.update_heap_pages(heap_pages) {
						trace!(
							target: "runtimes_cache",
							"heap_pages were changed. Reinstantiating the instance",
						);
						*result = create_versioned_wasm_runtime(ext, wasm_method, heap_pages);
						if let Err(ref err) = result {
							warn!(target: "runtimes_cache", "cannot create a runtime: {:?}", err);
						}
					}
				}
				result
			},
			Entry::Vacant(v) => {
				trace!(target: "runtimes_cache", "no instance found in cache, creating now.");
				let result = create_versioned_wasm_runtime(ext, wasm_method, heap_pages);
				if let Err(ref err) = result {
					warn!(target: "runtimes_cache", "cannot create a runtime: {:?}", err);
				}
				v.insert(result)
			}
		};

		result.as_mut()
			.map(|entry| (entry.runtime.as_mut(), &entry.version, code_hash))
			.map_err(|ref e| Error::InvalidCode(format!("{:?}", e)))
	}

	/// Invalidate the runtime for the given `wasm_method` and `code_hash`.
	///
	/// Invalidation of a runtime is useful when there was a `panic!` in native while executing it.
	/// The `panic!` maybe have brought the runtime into a poisoned state and so, it is better to
	/// invalidate this runtime instance.
	pub fn invalidate_runtime(
		&mut self,
		wasm_method: WasmExecutionMethod,
		code_hash: H256,
	) {
		// Just remove the instance, it will be re-created the next time it is requested.
		self.instances.remove(&(wasm_method, code_hash.into()));
	}
}

/// Create a wasm runtime with the given `code`.
pub fn create_wasm_runtime_with_code(
	wasm_method: WasmExecutionMethod,
	heap_pages: u64,
	code: &[u8],
) -> Result<Box<dyn WasmRuntime>, WasmError> {
	match wasm_method {
		WasmExecutionMethod::Interpreted =>
			wasmi_execution::create_instance(code, heap_pages)
				.map(|runtime| -> Box<dyn WasmRuntime> { Box::new(runtime) }),
		#[cfg(feature = "wasmtime")]
		WasmExecutionMethod::Compiled =>
			wasmtime::create_instance(code, heap_pages)
				.map(|runtime| -> Box<dyn WasmRuntime> { Box::new(runtime) }),
	}
}

fn create_versioned_wasm_runtime<E: Externalities>(
	ext: &mut E,
	wasm_method: WasmExecutionMethod,
	heap_pages: u64,
) -> Result<VersionedRuntime, WasmError> {
	let code = ext
		.original_storage(well_known_keys::CODE)
		.ok_or(WasmError::CodeNotFound)?;
	let mut runtime = create_wasm_runtime_with_code(wasm_method, heap_pages, &code)?;

	// Call to determine runtime version.
	let version_result = {
		// `ext` is already implicitly handled as unwind safe, as we store it in a global variable.
		let mut ext = AssertUnwindSafe(ext);

		// The following unwind safety assertion is OK because if the method call panics, the
		// runtime will be dropped.
		let mut runtime = AssertUnwindSafe(runtime.as_mut());
		crate::native_executor::safe_call(
			move || runtime.call(&mut **ext, "Core_version", &[])
		).map_err(|_| WasmError::Instantiation("panic in call to get runtime version".into()))?
	};
	let encoded_version = version_result
		.map_err(|e| WasmError::Instantiation(format!("failed to call \"Core_version\": {}", e)))?;
	let version = RuntimeVersion::decode(&mut encoded_version.as_slice())
		.map_err(|_| WasmError::Instantiation("failed to decode \"Core_version\" result".into()))?;

	Ok(VersionedRuntime {
		runtime,
		version,
	})
}
