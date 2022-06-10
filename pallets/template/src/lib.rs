#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

mod fees;

pub use fees::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	use codec::Codec;
	use codec::{EncodeLike, MaxEncodedLen};
	use frame_support::traits::{Currency, Get, Imbalance};
	use frame_support::{pallet_prelude::*, weights::Weight};
	use frame_system::{
		offchain::{SendTransactionTypes, SubmitTransaction},
		pallet_prelude::*,
		RawOrigin,
	};
	use pallet_treasury::{BalanceOf, PositiveImbalanceOf};
	use scale_info::TypeInfo;
	use sp_runtime::{
		offchain::{
			storage::StorageValueRef,
			storage_lock::{StorageLock, Time},
		},
		traits::{AtLeast32BitUnsigned, Convert, Saturating, Zero},
		Percent,
	};
	use sp_std::{fmt::Debug, vec, vec::Vec};
	use xcm::{latest::prelude::*, VersionedXcm};

	const DB_KEY_SUM: &[u8] = b"donations/txn-fee-sum";
	const DB_LOCK: &[u8] = b"donations/txn-sum-lock";

	/// Configure the pallet by specifying the parameters and types on which it depends.
	#[pallet::config]
	pub trait Config:
		frame_system::Config
		+ pallet_balances::Config
		+ pallet_treasury::Config
		+ pallet_xcm::Config
		+ SendTransactionTypes<Call<Self>>
	{
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		type BalancesEvent: From<<Self as frame_system::Config>::Event>
			+ TryInto<pallet_balances::Event<Self>>;
		type Balance: AtLeast32BitUnsigned
			+ Saturating
			+ Codec
			+ TypeInfo
			+ Default
			+ Debug
			+ Copy
			+ EncodeLike
			+ MaxEncodedLen
			+ From<<Self as pallet_balances::Config>::Balance>
			+ Into<BalanceOf<Self>>;

		type FeeCalculator: FeeCalculator<Self>;

		type AccountIdToMultiLocation: Convert<Self::AccountId, MultiLocation>;

		// weight of an xcm transaction to send to sequester
		#[pallet::constant]
		type SequesterTransferFee: Get<<Self as Config>::Balance>;

		// weight of an xcm transaction to send to sequester
		#[pallet::constant]
		type SequesterTransferWeight: Get<Weight>;

		// Transaction priority for the unsigned transactions
		#[pallet::constant]
		type UnsignedPriority: Get<TransactionPriority>;

		// Interval (in blocks) at which we send fees to sequester and reset the
		// txn-fee-sum variable
		#[pallet::constant]
		type SendInterval: Get<Self::BlockNumber>;

		// AccountID of the xcm account, which will be used to send funds to sequester
		// https://github.com/AcalaNetwork/Acala/blob/ded6de57234c4367401dbd758db609254c2e00e0/modules/evm/src/lib.rs#L229
		#[pallet::constant]
		type DonationsXCMAccount: Get<Self::AccountId>;

		#[pallet::constant]
		type TxnFeePercentage: Get<Percent>;
	}

	// The next block where an unsigned transaction will be considered valid
	#[pallet::type_value]
	pub(super) fn DefaultNextUnsigned<T: Config>() -> T::BlockNumber {
		T::BlockNumber::from(0u32)
	}
	#[pallet::storage]
	#[pallet::getter(fn next_unsigned_at)]
	pub(super) type NextUnsignedAt<T: Config> =
		StorageValue<_, T::BlockNumber, ValueQuery, DefaultNextUnsigned<T>>;

	#[pallet::storage]
	#[pallet::getter(fn fees_to_send)]
	pub(super) type FeesToSend<T: Config> = StorageValue<_, BalanceOf<T>, ValueQuery>;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Transaction fee has been sent from the treasury to Sequester with
		/// amount: Balance
		TxnFeeQueued(<T as Config>::Balance),
		TxnFeeSubsumed(BalanceOf<T>),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Error names should be descriptive.
		XcmExecutionFailed,
		/// Errors should have helpful documentation associated with them.
		FeeConvertFailed,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		// Each block, initiate an offchain worker to summarize the txn fees for that block,
		// and append that amount to a counter in local storage, which we will empty
		// when it is time to send the txn fees to Sequester.
		fn offchain_worker(block_number: T::BlockNumber) {
			let block_fee_sum = Self::calculate_fees_for_block();

			log::info!("total fees for block!!: {:?}", block_fee_sum);

			Self::update_storage(block_fee_sum);

			// send fees to sequester
			if (block_number % T::SendInterval::get()).is_zero() {
				Self::send_fees_to_sequester(block_number);
			}
		}
	}

	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;
		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			log::info!("validating unsigned transaction");
			if let Call::submit_unsigned { amount, block_num, .. } = call {
				// Discard solution not coming from the local OCW.
				match source {
					TransactionSource::Local | TransactionSource::InBlock => { /* allowed */ },
					_ => return InvalidTransaction::Call.into(),
				}

				// Reject outdated txns
				let next_unsigned_at = Self::next_unsigned_at();
				if &next_unsigned_at > block_num {
					return InvalidTransaction::Stale.into();
				}
				// Reject txns from the future
				let current_block = <frame_system::Pallet<T>>::block_number();
				if &current_block < block_num {
					return InvalidTransaction::Future.into();
				}

				log::info!("valid unsigned transaction -- sending {:?} to sequester", amount);

				ValidTransaction::with_tag_prefix("Donations")
					.priority(T::UnsignedPriority::get())
					// We don't propagate this. This can never be validated at a remote node.
					.propagate(false)
					.build()
			} else {
				InvalidTransaction::Call.into()
			}
		}
	}

	impl<T: Config> pallet_treasury::SpendFunds<T> for Pallet<T> {
		// Using as resource: https://github.com/paritytech/substrate/blob/ded44948e2d5a398abcb4e342b0513cb690961bb/frame/bounties/src/lib.rs
		fn spend_funds(
			budget_remaining: &mut BalanceOf<T>,
			imbalance: &mut PositiveImbalanceOf<T>,
			_total_weight: &mut Weight,
			_missed_any: &mut bool,
		) {
			log::info!("spend_funds triggered!");
			let fees_to_send = Self::fees_to_send();

			// TODO: Apply percentage

			let zero_bal: BalanceOf<T> = Zero::zero();

			log::info!(
				"fees_to_send: {:?} and budget remaining in treasury : {:?}",
				fees_to_send,
				*budget_remaining,
			);

			// valid fees to send
			if fees_to_send > zero_bal && *budget_remaining >= fees_to_send {
				*budget_remaining -= fees_to_send;

				let sequester_acc = T::DonationsXCMAccount::get();

				imbalance.subsume(T::Currency::deposit_creating(&sequester_acc, fees_to_send));
				// reset fee counter
				FeesToSend::<T>::set(zero_bal);
				Self::deposit_event(Event::TxnFeeSubsumed(fees_to_send));

				// xcm call via reserve_transfer_assets (https://github.com/paritytech/polkadot/blob/02d040ebd271a4b395b79277640878d4c768fb47/xcm/pallet-xcm/src/lib.rs#L536)
				let _ = Self::xcm_transfer_to_sequester(
					RawOrigin::Signed(sequester_acc).into(),
					fees_to_send,
				);
			}
			// *total_weight += <T as Config>::WeightInfo::spend_funds(bounties_len);
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		// TODO: calculate weight
		#[pallet::weight(10_000)]
		pub fn submit_unsigned(
			origin: OriginFor<T>,
			amount: <T as Config>::Balance,
			block_num: T::BlockNumber,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;

			// update storage to reject unsigned transactions until SendInterval blocks pass
			<NextUnsignedAt<T>>::put(block_num + T::SendInterval::get());

			let pending_fees = Self::fees_to_send();

			// add pending XCM transfer to Sequester here
			FeesToSend::<T>::set(pending_fees.saturating_add(amount.into()));

			Self::deposit_event(Event::TxnFeeQueued(amount));
			Ok(None.into())
		}

		// TODO: calculate weight
		#[pallet::weight(10_000)]
		pub fn xcm_transfer_to_sequester(
			origin: OriginFor<T>,
			amount: BalanceOf<T>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			log::info!("amount to send via xcm: {:?}", amount);

			// send to same account on Sequester
			let dest = who.clone();

			let origin_location = T::AccountIdToMultiLocation::convert(who.clone());
			let dst_location = T::AccountIdToMultiLocation::convert(dest.clone());

			let amount_u128 =
				TryInto::<u128>::try_into(amount).map_err(|_| Error::<T>::FeeConvertFailed)?;

			let weight = T::SequesterTransferWeight::get();
			let fee = T::SequesterTransferFee::get();

			let fee_u128 =
				TryInto::<u128>::try_into(fee).map_err(|_| Error::<T>::FeeConvertFailed)?;

			let mut assets = MultiAssets::new();

			let fee_asset = MultiAsset {
				id: AssetId::Concrete(MultiLocation::new(1, Junctions::Here)),
				fun: Fungibility::Fungible(fee_u128),
			};
			assets.push(fee_asset.clone());

			let msg: xcm::v2::Xcm<<T as frame_system::Config>::Call> =
				Xcm(vec![WithdrawAsset(assets)]);

			<T as pallet_xcm::Config>::XcmExecutor::execute_xcm_in_credit(
				origin_location,
				msg,
				weight,
				weight,
			)
			.ensure_complete()
			.map_err(|_| Error::<T>::XcmExecutionFailed)?;

			Ok(None.into())
		}
	}

	impl<T: Config> Pallet<T> {
		fn calculate_fees_for_block() -> <T as Config>::Balance {
			let events = <frame_system::Pallet<T>>::read_events_no_consensus();

			let mut curr_block_fee_sum = Zero::zero();

			let filtered_events = events.into_iter().filter_map(|event_record| {
				let balances_event = <T as Config>::BalancesEvent::from(event_record.event);
				balances_event.try_into().ok()
			});

			for event in filtered_events {
				<T as Config>::FeeCalculator::match_event(event, &mut curr_block_fee_sum);
			}

			curr_block_fee_sum
		}

		fn update_storage(block_fee_sum: <T as Config>::Balance) {
			// Use get/set instead of mutation to guarantee that we don't
			// hit any MutateStorageError::ConcurrentModification errors
			let mut lock = StorageLock::<Time>::new(&DB_LOCK);
			{
				let _guard = lock.lock();
				let val = StorageValueRef::persistent(&DB_KEY_SUM);
				match val.get::<<T as Config>::Balance>() {
					// initialize value
					Ok(None) => {
						log::info!(
							"initializing storage val and setting it to: {:?}",
							block_fee_sum
						);
						val.set(&block_fee_sum);
					},
					// update value
					Ok(Some(fetched_txn_fee_sum)) => {
						log::info!(
							"retrieved storage val: {:?} and adding : {:?}",
							fetched_txn_fee_sum,
							block_fee_sum
						);
						val.set(&fetched_txn_fee_sum.saturating_add(block_fee_sum));
					},
					_ => {},
				};
			}
		}

		fn send_fees_to_sequester(block_num: T::BlockNumber) {
			// get lock so that another ocw doesn't modify the value mid-send
			let mut lock = StorageLock::<Time>::new(&DB_LOCK);
			{
				let _guard = lock.lock();
				let val = StorageValueRef::persistent(&DB_KEY_SUM);
				let fees_to_send = val.get::<<T as Config>::Balance>();
				match fees_to_send {
					Ok(Some(fetched_fees)) => {
						let call = Call::<T>::submit_unsigned { amount: fetched_fees, block_num };
						let txn_res = SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(
							call.into(),
						);
						match txn_res {
							Ok(_) => {
								log::info!("resetting storage value");
								let zero_bal: <T as Config>::Balance = Zero::zero();
								val.set(&zero_bal);
							},
							Err(_) => {
								log::error!("Failed in offchain_unsigned_tx");
							},
						}
					},
					_ => {},
				};
			}
		}
	}
}
