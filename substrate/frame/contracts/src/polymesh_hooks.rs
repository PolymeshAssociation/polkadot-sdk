

pub trait PolymeshHooks<T: frame_system::Config> {
	fn check_call_permissions(
		caller: &T::AccountId,
	) -> frame_support::dispatch::DispatchResult;

	fn on_instantiate_transfer(
		caller: &T::AccountId,
		contract: &T::AccountId,
	) -> frame_support::dispatch::DispatchResult;

	#[cfg(feature = "runtime-benchmarks")]
	fn register_did(
		account: T::AccountId
	) -> frame_support::dispatch::DispatchResult;
}

pub struct DefaultPolymeshHooks;

impl<T: frame_system::Config> PolymeshHooks<T> for DefaultPolymeshHooks {
	fn check_call_permissions(
		_caller: &T::AccountId,
	) -> frame_support::dispatch::DispatchResult {
		Ok(())
	}

	fn on_instantiate_transfer(
		_caller: &T::AccountId,
		_contract: &T::AccountId,
	) -> frame_support::dispatch::DispatchResult {
		Ok(())
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn register_did(
		_account: T::AccountId
	) -> frame_support::dispatch::DispatchResult {
		Ok(())
	}
}

