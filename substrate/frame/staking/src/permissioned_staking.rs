use frame_support::{dispatch::DispatchResult, traits::fungible::Inspect};
use sp_runtime::Perbill;

use crate::{ActiveEraInfo, BalanceOf, Config};

/// A trait used by the staking pallet for permissioned staking.
///
/// A permissioned Substrate network can be configured to allow only a set of
/// identities to participate in staking. This trait is used to define the
/// behavior of the staking pallet in such a network.
pub trait PermissionedStaking<T: Config> {
	/// Onboard an account.
	#[cfg(any(feature = "testing", test))]
	fn onboard_account(_who: &T::AccountId) {}

	/// Permission a validator.
	#[cfg(any(feature = "testing", test))]
	fn permission_validator(_who: &T::AccountId) {}

	/// Setup stash and controller.
	#[cfg(any(feature = "runtime-benchmarks", test))]
	fn setup_stash_and_controller(_stash: &T::AccountId, _controller: &T::AccountId) {}

	/// Setup stash and controller.
	#[cfg(feature = "runtime-benchmarks")]
	fn setup_who_to_slash(_who_to_slash: Option<WhoToSlash>) {}

	/// Check if amount is under the existential deposit.
	fn reapable(amount: BalanceOf<T>) -> bool {
		amount < T::Currency::minimum_balance()
	}

	/// On validate hook.
	fn on_validate(_who: &T::AccountId, _commission: Perbill) -> DispatchResult {
		Ok(())
	}

	/// On chill hook.
	fn on_chill(_who: &T::AccountId) {}

	/// On nominate hook.
	fn on_nominate(_who: &T::AccountId) -> DispatchResult {
		Ok(())
	}

	/// Is the validator still compliant?
	fn is_validator_compliant(_who: &T::AccountId) -> bool {
		true
	}

	/// Is the nominator still compliant?
	fn is_nominator_compliant(_who: &T::AccountId) -> bool {
		true
	}

	/// Schedule reward payouts.
	fn schedule_payouts(_active_era: &ActiveEraInfo) -> DispatchResult {
		Ok(())
	}

	/// Who should be slashed?
	fn who_to_slash() -> Option<WhoToSlash> {
		Some(WhoToSlash::ValidatorAndNominator)
	}

	/// Is slashing enabled?
	fn is_slashing_enabled() -> bool {
		Self::who_to_slash().is_some()
	}

	/// Slash nominators?
	fn slash_nominators() -> bool {
		Self::who_to_slash() == Some(WhoToSlash::ValidatorAndNominator)
	}
}

impl<T: Config> PermissionedStaking<T> for () {}

/// Who should be slashed.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum WhoToSlash {
	/// Allow validators but not nominators to get slashed.
	Validator,
	/// Allow both validators and nominators to get slashed.
	ValidatorAndNominator,
}
