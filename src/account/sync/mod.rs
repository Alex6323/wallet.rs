use crate::account::{Account, AccountIdentifier};
use crate::address::{Address, AddressBuilder};
use crate::client::get_client;
use crate::message::{Message, Transfer};

use iota::transaction::{
    prelude::{
        Error as TransactionError, Input, Message as IotaMessage, MessageId, Output, Payload,
        SignatureLockedSingleOutput, Transaction, UTXOInput,
    },
    Vertex,
};
use slip10::path::BIP32Path;

use std::num::NonZeroU64;

mod input_selection;

/// Syncs addresses with the tangle.
/// The method ensures that the wallet local state has all used addresses plus an unused address.
///
/// To sync addresses for an account from scratch, `address_index` = 0 and `gap_limit` = 20 should be provided.
/// To sync addresses from the latest address, `address_index` = latest address index and `gap_limit` = 1 should be provided.
///
/// # Arguments
///
/// * `address_index` The address index.
/// * `gap_limit` Number of addresses indexes that are generated.
///
/// # Return value
///
/// Returns a (addresses, messages) tuples representing the address history up to latest unused address,
/// and the messages associated with the addresses.
///
async fn sync_addresses(
    account: &'_ Account,
    address_index: usize,
    gap_limit: Option<usize>,
) -> crate::Result<(Vec<Address>, Vec<IotaMessage>)> {
    let mut address_index = address_index;

    let client = get_client(account.client_options());
    let gap_limit = gap_limit.unwrap_or(20);

    let mut generated_addresses = vec![];
    let mut found_transactions = vec![];
    loop {
        let mut generated_iota_addresses = vec![];
        for i in address_index..(address_index + gap_limit) {
            // generate both `public` and `internal (change)` addresses
            generated_iota_addresses.push(crate::address::get_iota_address(&account, i, false)?);
            generated_iota_addresses.push(crate::address::get_iota_address(&account, i, true)?);
        }

        let curr_found_transactions = client
            .get_transactions()
            .addresses(&generated_iota_addresses[..])
            .get()?;
        found_transactions.extend(curr_found_transactions.iter().cloned());

        let generated_addresses_outputs =
            client.get_addresses_balance(&generated_iota_addresses[..])?;

        for iota_address in generated_iota_addresses {
            let balance = generated_addresses_outputs
                .iter()
                .filter(|output| output.address == iota_address)
                .fold(0, |acc, output| acc + output.amount);
            let address = AddressBuilder::new()
                .address(iota_address)
                .key_index(address_index)
                .balance(balance)
                .build()?;
            generated_addresses.push(address);
            address_index += 1;
        }

        if found_transactions.is_empty()
            && generated_addresses_outputs.iter().all(|o| o.amount == 0)
        {
            break;
        }
    }

    Ok((generated_addresses, found_transactions))
}

/// Syncs transactions with the tangle.
/// The method should ensures that the wallet local state has transactions associated with the address history.
async fn sync_transactions<'a>(
    account: &'a Account,
    new_message_ids: Vec<MessageId>,
) -> crate::Result<Vec<Message>> {
    let mut messages: Vec<Message> = account.messages().to_vec();

    // sync `broadcasted` state
    messages
        .iter_mut()
        .filter(|message| !message.broadcasted() && new_message_ids.contains(message.message_id()))
        .for_each(|message| {
            message.set_broadcasted(true);
        });

    // sync `confirmed` state
    let mut unconfirmed_messages: Vec<&mut Message> = messages
        .iter_mut()
        .filter(|message| !message.confirmed())
        .collect();
    let client = get_client(account.client_options());
    let unconfirmed_transaction_ids: Vec<MessageId> = unconfirmed_messages
        .iter()
        .map(|message| *message.message_id())
        .collect();
    let confirmed_states = client.is_confirmed(&unconfirmed_transaction_ids[..])?;
    for (message, confirmed) in unconfirmed_messages
        .iter_mut()
        .zip(confirmed_states.values())
    {
        if *confirmed {
            message.set_confirmed(true);
        }
    }

    // get new transactions
    let found_messages = client
        .get_transactions()
        .hashes(&new_message_ids[..])
        .get()?;
    let mut ids_iter = new_message_ids.iter();

    for message in found_messages {
        let message_id = ids_iter.next().unwrap();
        messages.push(Message::from_iota_message(&message).unwrap());
    }

    Ok(messages)
}

/// Account sync helper.
pub struct AccountSynchronizer<'a> {
    account: &'a mut Account,
    address_index: usize,
    gap_limit: Option<usize>,
    skip_persistance: bool,
}

impl<'a> AccountSynchronizer<'a> {
    /// Initialises a new instance of the sync helper.
    pub(super) fn new(account: &'a mut Account) -> Self {
        let address_index = account.addresses().len();
        Self {
            account,
            address_index,
            gap_limit: None,
            skip_persistance: false,
        }
    }

    /// Number of address indexes that are generated.
    pub fn gap_limit(mut self, limit: usize) -> Self {
        self.gap_limit = Some(limit);
        self
    }

    /// Skip write to the database.
    pub fn skip_persistance(mut self) -> Self {
        self.skip_persistance = true;
        self
    }

    /// Syncs account with the tangle.
    /// The account syncing process ensures that the latest metadata (balance, transactions)
    /// associated with an account is fetched from the tangle and is stored locally.
    pub async fn execute(self) -> crate::Result<SyncedAccount> {
        let client = get_client(self.account.client_options());
        let mut addresses = vec![];
        for address in self.account.addresses() {
            addresses.push(address.address().clone());
        }

        let (found_addresses, found_messages) =
            sync_addresses(self.account, self.address_index, self.gap_limit).await?;

        let mut new_message_ids = vec![];
        for found_message in found_messages {
            if !self.account.messages().iter().any(
                |message| message.message_id() == found_message.trunk(), /* TODO hash instead of trunk */
            ) {
                new_message_ids.push(*found_message.trunk()); // TODO hash instead of trunk
            }
        }

        let messages = sync_transactions(self.account, new_message_hashes).await?;

        self.account.set_messages(messages);
        self.account.set_addresses(found_addresses);
        let storage_adapter = crate::storage::get_adapter()?;
        storage_adapter.set(
            self.account.id().into(),
            serde_json::to_string(&self.account)?,
        )?;

        let synced_account = SyncedAccount {
            account_id: *self.account.id(),
            deposit_address: self.account.latest_address().unwrap().clone(),
        };
        Ok(synced_account)
    }
}

/// Data returned from account synchronization.
pub struct SyncedAccount {
    account_id: [u8; 32],
    deposit_address: Address,
}

impl SyncedAccount {
    /// The account's deposit address.
    pub fn deposit_address(&self) -> &Address {
        &self.deposit_address
    }

    /// Selects input addresses for a value transaction.
    /// The method ensures that the recipient address doesn’t match any of the selected inputs or the remainder address.
    ///
    /// # Arguments
    ///
    /// * `threshold` Amount user wants to spend.
    /// * `address` Recipient address.
    ///
    /// # Return value
    ///
    /// Returns a (addresses, address) tuple representing the selected input addresses and the remainder address if needed.
    fn select_inputs<'a>(
        &self,
        threshold: u64,
        account: &'a Account,
        address: &'a Address,
    ) -> crate::Result<(Vec<Address>, Option<&'a Address>)> {
        let mut available_addresses = vec![];
        let available_addresses_iter = account.addresses().iter().filter(|a| a != &address);
        for available_address in available_addresses_iter {
            available_addresses.push(available_address.clone());
        }
        let addresses = input_selection::select_input(threshold, &mut available_addresses)?;
        let remainder = if addresses.iter().fold(0, |acc, a| acc + a.balance()) > threshold {
            account.latest_address()
        } else {
            None
        };
        Ok((addresses, remainder))
    }

    /// Send messages.
    pub async fn transfer(&self, transfer_obj: Transfer) -> crate::Result<Message> {
        // validate the transfer
        if *transfer_obj.amount() == 0 {
            return Err(anyhow::anyhow!("amount can't be zero"));
        }

        // prepare the transfer getting some needed objects and values
        let value: u64 = *transfer_obj.amount();
        let account_id: AccountIdentifier = self.account_id.clone().into();
        let adapter = crate::storage::get_adapter()?;
        let mut account = crate::storage::get_account(account_id.clone())?;
        let client = get_client(account.client_options());

        // select the input addresses and check if a remainder address is needed
        let (input_addresses, remainder_address) =
            self.select_inputs(*transfer_obj.amount(), &account, transfer_obj.address())?;

        let mut utxo_outputs_addresses = vec![];
        for utxo_output in &input_addresses {
            utxo_outputs_addresses.push(utxo_output.address().clone());
        }
        let utxos = client
            .get_outputs()
            .addresses(&utxo_outputs_addresses[..])
            .get()?;

        let mut indexed_utxo_inputs: Vec<(Input, BIP32Path)> = vec![];
        let mut utxo_outputs: Vec<Output> = vec![];
        let mut current_output_sum = 0;
        for utxo in utxos {
            indexed_utxo_inputs.push((
                UTXOInput::new(utxo.producer, utxo.output_index)
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?
                    .into(),
                BIP32Path::from_str("").map_err(|e| anyhow::anyhow!(e.to_string()))?,
            ));
            let utxo_amount = if current_output_sum + utxo.amount > value {
                value - utxo.amount
            } else {
                utxo.amount
            };
            let utxo_address = if current_output_sum == value {
                remainder_address
                    .map(|a| a.address().clone())
                    .expect("remainder address not defined")
            } else {
                utxo.address
            };
            current_output_sum += utxo.amount;
            utxo_outputs.push(
                SignatureLockedSingleOutput::new(
                    utxo_address,
                    NonZeroU64::new(utxo_amount)
                        .ok_or_else(|| anyhow::anyhow!("invalid amount"))?,
                )
                .into(),
            );
        }

        let (parent1, parent2) = client.get_tips()?;

        let stronghold_account =
            crate::with_stronghold(|stronghold| stronghold.account_get_by_id(account.id()))?;
        let transaction_res: Result<Transaction, TransactionError> = stronghold_account
            .with_transaction_builder(|builder| {
                builder
                    .set_outputs(utxo_outputs)
                    .set_inputs(indexed_utxo_inputs)
                    .build()
            });
        let transaction = transaction_res.map_err(|e| anyhow::anyhow!(format!("{:?}", e)))?;
        let message = IotaMessage::builder()
            .parent1(parent1)
            .parent2(parent2)
            .payload(Payload::Transaction(Box::new(transaction)))
            .build()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        let attached = client.post_messages(&[message])?;
        let messages: Vec<Message> = client
            .get_messages()
            .hashes(&attached[..])
            .get()?
            .iter()
            .map(|message| Message::from_iota_message(&message).unwrap())
            .collect();

        let message = messages.first().unwrap().clone();
        account.append_messages(messages);
        adapter.set(account_id, serde_json::to_string(&account)?)?;

        Ok(message)
    }

    /// Retry messages.
    pub fn retry(&self, message_id: &MessageId) -> crate::Result<Message> {
        let account: Account = crate::storage::get_account(self.account_id.clone().into())?;
        let message = account
            .get_message(message_id)
            .ok_or_else(|| anyhow::anyhow!("transaction with the given id not found"));
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use crate::account_manager::AccountManager;
    use crate::client::ClientOptionsBuilder;

    #[tokio::test]
    async fn account_sync() -> crate::Result<()> {
        let manager = AccountManager::new();
        let client_options =
            ClientOptionsBuilder::node("https://nodes.devnet.iota.org:443")?.build();
        let mut account = manager
            .create_account(client_options)
            .alias("alias")
            .initialise()?;

        let synced_accounts = account.sync().execute().await?;

        Ok(())
    }
}
