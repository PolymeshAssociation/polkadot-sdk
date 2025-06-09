use frame_support::{
	pallet_prelude::DispatchResult,
	traits::{LockIdentifier, WithdrawReasons},
};

use crate::{
	pallet::{Error, Locks, Pallet, LockableCurrencyExt},
	BalanceLock, Config,
};

impl<T: Config> LockableCurrencyExt<T::AccountId, T::Balance> for Pallet<T> {
	fn reduce_lock(lock_id: LockIdentifier, acc_id: &AccountId, amount: Self::Balance) -> DispatchResult {
		if amount.is_zero() {
			return Ok(());
		}

		let mut locks = Locks::<T>::get(acc_id);

		let lock_id_index = locks
			.iter()
			.position(|lock| lock.id == lock_id)
			.ok_or(Error::<T>::LockIdentifierNotFound)?;

		let mut balance_lock = &mut locks[lock_id_index];
		balance_lock.amount =
			balance_lock.amount.checked_sub(amount).ok_or(Error::<T>::InsufficientBalance)?;

		if balance_lock.amount.is_zero() {
			locks.swap_remove(lock_id_index);
		}

		Self::update_locks(acc_id, &locks)?;

		Ok(())
	}

	fn increase_lock(
		lock_id: LockIdentifier,
		acc_id: &AccountId,
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
					let mut balance_lock = &mut locks[lock_id_index];
					balance_lock.amount =
						balance_lock.amount.checked_add(amount).ok_or(Error::<T>::Overflow)?;
					balance_lock.reasons = balance_lock.reasons | withdraw_reasons.into();
					balance_lock.amount
				},
				None => {
					let balance_lock =
						BalanceLock { id: lock_id, amount, reasons: withdraw_reasons.into() };
					locks.push(balance_lock);
					amount
				},
			}
		};

		check_sum(amount)?;

		Self::update_locks(acc_id, &locks)?;

		Ok(())
	}
}
