use frame_support::{
	pallet_prelude::DispatchResult,
	traits::{
		tokens::{fungible, Preservation::Expendable},
		LockIdentifier, LockableCurrency, WithdrawReasons,
	},
};
use sp_runtime::traits::{CheckedAdd, CheckedSub, Zero};

use crate::{
	pallet::{Error, Event, Locks, Pallet},
	BalanceLock, Config,
};

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	pub(crate) fn transfer_core(
		source: &T::AccountId,
		dest: &T::AccountId,
		amount: T::Balance,
		memo: Option<T::Memo>,
	) -> DispatchResult {
		<Self as fungible::Mutate<_>>::transfer(source, dest, amount, Expendable)?;
		Self::deposit_event(Event::TransferWithMemo {
			from: source.clone(),
			to: dest.clone(),
			amount,
			memo,
		});
		Ok(())
	}
}

pub trait LockableCurrencyExt<AccountId, Balance>:
	LockableCurrency<AccountId, Balance = Balance>
{
	/// Reduces the locked amount under `lock_id` for `acc_id`.
	fn reduce_lock(lock_id: LockIdentifier, acc_id: &AccountId, amount: Balance) -> DispatchResult;

	/// Increases the locked amount under `lock_id` for `acc_id`.
	fn increase_lock(
		lock_id: LockIdentifier,
		acc_id: &AccountId,
		amount: Balance,
		withdraw_reasons: WithdrawReasons,
		check_sum: impl FnOnce(Balance) -> DispatchResult,
	) -> DispatchResult;
}

impl<T: Config> LockableCurrencyExt<T::AccountId, T::Balance> for Pallet<T> {
	fn reduce_lock(
		lock_id: LockIdentifier,
		acc_id: &T::AccountId,
		amount: Self::Balance,
	) -> DispatchResult {
		if amount.is_zero() {
			return Ok(());
		}

		let mut locks = Locks::<T>::get(acc_id);

		let lock_id_index = locks
			.iter()
			.position(|lock| lock.id == lock_id)
			.ok_or(Error::<T>::LockIdentifierNotFound)?;

		let balance_lock = &mut locks[lock_id_index];
		balance_lock.amount = balance_lock
			.amount
			.checked_sub(&amount)
			.ok_or(Error::<T>::InsufficientBalance)?;

		if balance_lock.amount.is_zero() {
			locks.swap_remove(lock_id_index);
		}

		Self::update_locks(acc_id, &locks);

		Ok(())
	}

	fn increase_lock(
		lock_id: LockIdentifier,
		acc_id: &T::AccountId,
		amount: Self::Balance,
		withdraw_reasons: WithdrawReasons,
		check_sum: impl FnOnce(Self::Balance) -> DispatchResult,
	) -> DispatchResult {
		if amount.is_zero() || withdraw_reasons.is_empty() {
			return Ok(());
		}

		let mut locks = Locks::<T>::get(acc_id);

		let amount = {
			match locks.iter().position(|lock| lock.id == lock_id) {
				Some(lock_id_index) => {
					let balance_lock = &mut locks[lock_id_index];
					balance_lock.amount =
						balance_lock.amount.checked_add(&amount).ok_or(Error::<T>::Overflow)?;
					balance_lock.reasons = balance_lock.reasons | withdraw_reasons.into();
					balance_lock.amount
				},
				None => {
					let balance_lock =
						BalanceLock { id: lock_id, amount, reasons: withdraw_reasons.into() };
					locks.try_push(balance_lock).map_err(|_| Error::<T>::MaxLocksExceeded)?;
					amount
				},
			}
		};

		check_sum(amount)?;

		Self::update_locks(acc_id, &locks);

		Ok(())
	}
}
