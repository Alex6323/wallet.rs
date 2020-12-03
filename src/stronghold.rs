//! Stronghold interface abstractions over an account

use crate::account::{Account, AccountIdentifier};
use futures::future::RemoteHandle;
use iota_stronghold::{ClientMsg, RecordHint, RecordId, SHRequest, SHResults, VaultId};
use once_cell::sync::{Lazy, OnceCell};
use riker::actors::*;
use riker_patterns::ask::ask;

use std::{
    collections::HashMap,
    convert::TryInto,
    fmt::{Display, Formatter, Result as FmtResult},
    path::{Path, PathBuf},
    sync::{
        mpsc::{
            channel as mpsc_channel, Receiver as MpscReceiver, RecvTimeoutError,
            Sender as MpscSender,
        },
        Arc, Mutex,
    },
    time::Duration,
};

static PASSWORD_STORE: OnceCell<Arc<Mutex<HashMap<PathBuf, String>>>> = OnceCell::new();

const SEED_HINT: &str = "IOTA_WALLET_SEED";
const ACCOUNT_HINT: &str = "IOTA_WALLET_ACCOUNT";
const TIMEOUT: Duration = Duration::from_millis(500);

/// wait for a stronghold result through the mpsc channel
#[macro_export]
macro_rules! wait_for_result {
    ($self:ident, $a:pat, $b:block) => {{
        let result_rx = $self.result_rx.lock().unwrap();
        let result = result_rx.recv_timeout(TIMEOUT)?;
        if let $a = result {
            $b
        } else {
            return Err(Error::UnexpectedResult(result));
        }
    }};
    ($self:ident, $a:pat, $b:block, $r:expr) => {{
        let result_rx = $self.result_rx.lock().unwrap();
        let result = result_rx.recv_timeout(TIMEOUT)?;
        if let $a = result {
            $b
        } else {
            return Err($r);
        }
    }};
}

fn set_password<S: AsRef<Path>, P: Into<String>>(snapshot_path: S, password: P) {
    let mut passwords = PASSWORD_STORE.get_or_init(Default::default).lock().unwrap();
    passwords.insert(snapshot_path.as_ref().to_path_buf(), password.into());
}

fn get_password<P: AsRef<Path>>(snapshot_path: P) -> Option<String> {
    let passwords = PASSWORD_STORE.get_or_init(Default::default).lock().unwrap();
    passwords
        .get(&snapshot_path.as_ref().to_path_buf())
        .cloned()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`{0}`")]
    Timeout(#[from] RecvTimeoutError),
    #[error("account id isn't a valid record hint")]
    InvalidAccountIdentifier,
    #[error("must provide account id instead of string")]
    AccountIdMustBeString,
    #[error("`{0}`")]
    StrongholdError(#[from] iota_stronghold::Error),
    #[error("account not found")]
    AccountNotFound,
    #[error("snapshot doesn't have accounts")]
    EmptySnapshot,
    #[error("unexpected stronghold response type: `{0}`")]
    UnexpectedResult(StrongholdResult),
    #[error("failed to perform action: `{0}`")]
    FailedToPerformAction(String),
}

pub type Result<T> = std::result::Result<T, Error>;

type StrongholdRemoteHandle = RemoteHandle<std::result::Result<StrongholdResponse, String>>;

#[derive(Debug, Clone)]
pub enum Request {
    LoadSnapshot(PathBuf, String),
    CreateSnapshot(PathBuf, String),
    GetAccount(AccountIdentifier),
    GetAccounts,
    StoreAccount(AccountIdentifier, String),
    RemoveAccount(AccountIdentifier),
}

enum Crypto {
    GenAddress,
}

#[derive(Debug)]
pub enum StrongholdResult {
    ReadRecord(Vec<u8>),
    ListIds(Vec<(RecordId, RecordHint)>),
    CreatedVault(VaultId),
    ReadSnapshot(Vec<VaultId>),
    Error(String),
}

impl Display for StrongholdResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Clone)]
enum StrongholdResponse {
    Accounts(Vec<Vec<u8>>),
    Account(Vec<u8>),
    StoredAccount,
    RemovedAccount,
    LoadedSnapshot,
    CreatedSnapshot,
}

#[actor(SHResults)]
struct StrongholdResultReceiver {
    channel: ChannelRef<SHResults>,
    result_tx: Arc<Mutex<MpscSender<StrongholdResult>>>,
}

impl
    ActorFactoryArgs<(
        ChannelRef<SHResults>,
        Arc<Mutex<MpscSender<StrongholdResult>>>,
    )> for StrongholdResultReceiver
{
    fn create_args(
        (channel, result_tx): (
            ChannelRef<SHResults>,
            Arc<Mutex<MpscSender<StrongholdResult>>>,
        ),
    ) -> Self {
        StrongholdResultReceiver { channel, result_tx }
    }
}

impl StrongholdResultReceiver {
    fn receive_response(
        &mut self,
        ctx: &Context<StrongholdResultReceiverMsg>,
        msg: SHResults,
    ) -> Result<()> {
        println!("response: {:?}", msg);
        let result_tx = self.result_tx.lock().unwrap();
        match msg {
            SHResults::ReturnRebuild(vaults, vault_records) => {
                result_tx
                    .send(StrongholdResult::ReadSnapshot(vaults))
                    .unwrap();
            }
            SHResults::ReturnList(records) => {
                result_tx.send(StrongholdResult::ListIds(records)).unwrap();
            }
            SHResults::ReturnCreate(vault_id, record_id) => {
                result_tx
                    .send(StrongholdResult::CreatedVault(vault_id))
                    .unwrap();
                println!("sent vault");
            }
            SHResults::ReturnInit(vault_id, record_id) => {}
            SHResults::ReturnRead(record) => {
                result_tx
                    .send(StrongholdResult::ReadRecord(record))
                    .unwrap();
            }
        }
        Ok(())
    }
}

impl Actor for StrongholdResultReceiver {
    type Msg = StrongholdResultReceiverMsg;

    // set up the channel.
    fn pre_start(&mut self, ctx: &Context<Self::Msg>) {
        let sub = Box::new(ctx.myself());
        let topic = Topic::from("external");
        self.channel.tell(Subscribe { actor: sub, topic }, None);
    }

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        self.receive(ctx, msg, sender);
    }
}

impl Receive<SHResults> for StrongholdResultReceiver {
    type Msg = StrongholdResultReceiverMsg;

    fn receive(&mut self, ctx: &Context<Self::Msg>, msg: SHResults, sender: Sender) {
        let _ = self.receive_response(ctx, msg);
    }
}

#[actor(Request)]
struct WalletStronghold {
    result_rx: Arc<Mutex<MpscReceiver<StrongholdResult>>>,
    seed_vault: Option<VaultId>,
    accounts_vault: Option<VaultId>,
}

impl ActorFactoryArgs<Arc<Mutex<MpscReceiver<StrongholdResult>>>> for WalletStronghold {
    fn create_args(result_rx: Arc<Mutex<MpscReceiver<StrongholdResult>>>) -> Self {
        WalletStronghold {
            result_rx,
            seed_vault: None,
            accounts_vault: None,
        }
    }
}

impl Actor for WalletStronghold {
    type Msg = WalletStrongholdMsg;

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        self.receive(ctx, msg, sender);
    }
}

fn account_id_to_record_id(account_id: AccountIdentifier) -> Result<RecordId> {
    let account_id_str = match account_id {
        AccountIdentifier::Id(id) => id,
        AccountIdentifier::Index(_) => {
            return Err(Error::AccountIdMustBeString);
        }
    };
    let id: RecordId = account_id_str.as_bytes()[0..24]
        .try_into()
        .map_err(|_| Error::InvalidAccountIdentifier)?;
    Ok(id)
}

impl WalletStronghold {
    fn clear_state(&mut self) {
        self.seed_vault = None;
        self.accounts_vault = None;
    }

    fn receive_message(
        &mut self,
        ctx: &Context<WalletStrongholdMsg>,
        msg: Request,
    ) -> Result<StrongholdResponse> {
        let stronghold_client = ctx
            .select("/user/stronghold-internal/")
            .expect("failed to select stronghold actor");
        match msg {
            Request::LoadSnapshot(snapshot_path, password) => {
                self.clear_state();
                set_password(&snapshot_path, &password);

                // read snapshot
                stronghold_client.try_tell(
                    ClientMsg::SHRequest(SHRequest::ReadSnapshot(
                        password,
                        None,
                        Some(snapshot_path),
                    )),
                    None,
                );
                wait_for_result!(self, StrongholdResult::ReadSnapshot(vaults), {
                    // search vault with the seed and vault with the accounts
                    for vault in vaults.iter() {
                        stronghold_client
                            .try_tell(ClientMsg::SHRequest(SHRequest::ListIds(*vault)), None);
                        let seed_hint = RecordHint::new(SEED_HINT).unwrap();
                        let account_hint = RecordHint::new(ACCOUNT_HINT).unwrap();
                        wait_for_result!(self, StrongholdResult::ListIds(records), {
                            if records.iter().any(|(_, hint)| hint == &seed_hint) {
                                self.seed_vault = Some(*vault);
                            }
                            if records.iter().any(|(_, hint)| hint == &account_hint) {
                                self.accounts_vault = Some(*vault);
                            }
                            if self.seed_vault.is_some() && self.accounts_vault.is_some() {
                                break;
                            }
                        });
                    }

                    if self.seed_vault.is_none() {
                        stronghold_client
                            .try_tell(ClientMsg::SHRequest(SHRequest::CreateNewVault), None);
                        wait_for_result!(self, StrongholdResult::CreatedVault(vault_id), {
                            self.seed_vault = Some(vault_id);
                        });
                    }
                    if self.accounts_vault.is_none() {
                        self.accounts_vault = Some(self.seed_vault.unwrap());
                    }
                    Ok(StrongholdResponse::LoadedSnapshot)
                })
            }
            Request::CreateSnapshot(snapshot_path, password) => {
                self.clear_state();
                set_password(snapshot_path, password);

                stronghold_client.try_tell(ClientMsg::SHRequest(SHRequest::CreateNewVault), None);
                wait_for_result!(self, StrongholdResult::CreatedVault(vault_id), {
                    self.seed_vault = Some(vault_id);
                    self.accounts_vault = Some(vault_id);
                    Ok(StrongholdResponse::CreatedSnapshot)
                })
            }
            Request::GetAccount(account_id) => {
                stronghold_client.try_tell(
                    ClientMsg::SHRequest(SHRequest::ReadData(
                        self.accounts_vault.unwrap(),
                        Some(account_id_to_record_id(account_id)?),
                    )),
                    None,
                );
                wait_for_result!(
                    self,
                    StrongholdResult::ReadRecord(record),
                    { Ok(StrongholdResponse::Account(record)) },
                    Error::AccountNotFound
                )
            }
            Request::GetAccounts => {
                let vault_id = self.accounts_vault.ok_or_else(|| Error::EmptySnapshot)?;
                stronghold_client
                    .try_tell(ClientMsg::SHRequest(SHRequest::ListIds(vault_id)), None);
                wait_for_result!(self, StrongholdResult::ListIds(record_pairs), {
                    let mut accounts = vec![];
                    let account_hint = RecordHint::new(ACCOUNT_HINT).unwrap();
                    for (id, hint) in record_pairs {
                        if hint == account_hint {
                            stronghold_client.try_tell(
                                ClientMsg::SHRequest(SHRequest::ReadData(
                                    self.accounts_vault.unwrap(),
                                    Some(id),
                                )),
                                None,
                            );
                            wait_for_result!(
                                self,
                                StrongholdResult::ReadRecord(record),
                                {
                                    accounts.push(record);
                                },
                                Error::AccountNotFound
                            );
                        }
                    }
                    Ok(StrongholdResponse::Accounts(accounts))
                })
            }
            Request::StoreAccount(account_id, account) => {
                stronghold_client.try_tell(
                    ClientMsg::SHRequest(SHRequest::WriteData(
                        self.accounts_vault.unwrap(),
                        Some(account_id_to_record_id(account_id)?),
                        account.as_bytes().to_vec(),
                        RecordHint::new(ACCOUNT_HINT).unwrap(),
                    )),
                    None,
                );
                Ok(StrongholdResponse::StoredAccount)
            }
            Request::RemoveAccount(account_id) => {
                let account_record_id = account_id_to_record_id(account_id)?;
                stronghold_client.try_tell(
                    ClientMsg::SHRequest(SHRequest::RevokeData(
                        self.accounts_vault.unwrap(),
                        account_record_id,
                    )),
                    None,
                );
                Ok(StrongholdResponse::RemovedAccount)
            }
        }
    }
}

impl Receive<Request> for WalletStronghold {
    type Msg = WalletStrongholdMsg;

    fn receive(&mut self, ctx: &Context<Self::Msg>, msg: Request, sender: Sender) {
        let res = self.receive_message(ctx, msg);
        sender
            .as_ref()
            .unwrap()
            .try_tell(res.map_err(|e| e.to_string()), Some(ctx.myself().into()))
            .unwrap();
    }
}

struct ActorRuntime {
    system: ActorSystem,
    stronghold_channel: ChannelRef<SHResults>,
    stronghold_actor: ActorRef<WalletStrongholdMsg>,
}

fn actor_runtime() -> &'static ActorRuntime {
    static SYSTEM: Lazy<ActorRuntime> = Lazy::new(|| {
        let system = ActorSystem::new().unwrap();
        let (system, stronghold_channel) = iota_stronghold::init_stronghold(system);
        let (result_tx, result_rx) = mpsc_channel();
        let stronghold_result_receiver_actor = system
            .actor_of_args::<StrongholdResultReceiver, _>(
                "wallet-stronghold-result-receiver",
                (stronghold_channel.clone(), Arc::new(Mutex::new(result_tx))),
            )
            .expect("failed to initialise stronghold actor");
        let stronghold_actor = system
            .actor_of_args::<WalletStronghold, _>(
                "wallet-stronghold",
                Arc::new(Mutex::new(result_rx)),
            )
            .expect("failed to initialise stronghold actor");
        ActorRuntime {
            system,
            stronghold_channel,
            stronghold_actor,
        }
    });
    &SYSTEM
}

pub async fn load_or_create<S: AsRef<Path>, P: Into<String>>(
    snapshot_path: S,
    password: P,
) -> Result<()> {
    let runtime = actor_runtime();

    if snapshot_path.as_ref().exists() {
        let message = Request::LoadSnapshot(snapshot_path.as_ref().to_path_buf(), password.into());
        let handle: StrongholdRemoteHandle =
            ask(&runtime.system, &runtime.stronghold_actor, message);
        let res = handle.await.map_err(|e| Error::FailedToPerformAction(e))?;
        if let StrongholdResponse::LoadedSnapshot = res {
            Ok(())
        } else {
            Err(Error::FailedToPerformAction(format!("{:?}", res)))
        }
    } else {
        let message =
            Request::CreateSnapshot(snapshot_path.as_ref().to_path_buf(), password.into());
        let handle: StrongholdRemoteHandle =
            ask(&runtime.system, &runtime.stronghold_actor, message);
        let res = handle.await.map_err(|e| Error::FailedToPerformAction(e))?;
        if let StrongholdResponse::CreatedSnapshot = res {
            Ok(())
        } else {
            Err(Error::FailedToPerformAction(format!("{:?}", res)))
        }
    }
}

pub async fn do_crypto(account: &Account) -> Result<()> {
    Ok(())
}

pub async fn get_accounts(storage_path: &PathBuf) -> Result<Vec<String>> {
    let runtime = actor_runtime();

    let message = Request::GetAccounts;
    let handle: StrongholdRemoteHandle = ask(&runtime.system, &runtime.stronghold_actor, message);
    let res = handle.await.map_err(|e| Error::FailedToPerformAction(e))?;
    if let StrongholdResponse::Accounts(accounts) = res {
        Ok(accounts
            .into_iter()
            .map(|acc| String::from_utf8_lossy(&acc).to_string())
            .collect())
    } else {
        Err(Error::FailedToPerformAction(format!("{:?}", res)))
    }
}

pub async fn get_account(storage_path: &PathBuf, account_id: AccountIdentifier) -> Result<String> {
    let runtime = actor_runtime();

    let message = Request::GetAccount(account_id);
    let handle: StrongholdRemoteHandle = ask(&runtime.system, &runtime.stronghold_actor, message);
    let res = handle.await.map_err(|e| Error::FailedToPerformAction(e))?;
    if let StrongholdResponse::Account(account) = res {
        Ok(String::from_utf8_lossy(&account).to_string())
    } else {
        Err(Error::FailedToPerformAction(format!("{:?}", res)))
    }
}

pub async fn store_account(
    storage_path: &PathBuf,
    account_id: AccountIdentifier,
    account: String,
) -> Result<()> {
    let runtime = actor_runtime();

    let message = Request::StoreAccount(account_id, account);
    let handle: StrongholdRemoteHandle = ask(&runtime.system, &runtime.stronghold_actor, message);
    let res = handle.await.map_err(|e| Error::FailedToPerformAction(e))?;
    if let StrongholdResponse::StoredAccount = res {
        Ok(())
    } else {
        Err(Error::FailedToPerformAction(format!("{:?}", res)))
    }
}

pub async fn remove_account(storage_path: &PathBuf, account_id: AccountIdentifier) -> Result<()> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use crate::account::AccountIdentifier;
    use std::path::PathBuf;
    #[tokio::test]
    async fn write_and_read() -> super::Result<()> {
        let snapshot_path: PathBuf = "./snapshot-test".into();
        super::load_or_create(&snapshot_path, "password").await?;

        let id = AccountIdentifier::Id(String::from_utf8_lossy(&[0; 32]).to_string());
        let account = "account data".to_string();
        println!("initialized");
        super::store_account(&snapshot_path, id.clone(), account.clone()).await?;
        let stored_account = super::get_account(&snapshot_path, id).await?;
        assert_eq!(stored_account, account);

        Ok(())
    }
}
