use std::collections::HashSet;
use std::future::Future;
use std::str::FromStr;

use crate::amms::amm::AMM;
use crate::amms::error::{AMMError, BatchContractError};
use crate::amms::factory::{AutomatedMarketMakerFactory, DiscoverySync};
use crate::amms::get_token_decimals;
use crate::amms::uniswap_v3::{tick_to_word, Info};
use crate::amms::uniswap_v4::lense::{
    decode_liquidity_gross_and_net, get_tick_bitmap_slot, get_tick_info_slot,
};
use crate::amms::uniswap_v4::IPoolManager::{
    swapCall, IPoolManagerCalls, IPoolManagerInstance, PoolKey,
};
use crate::amms::{amm::AutomatedMarketMaker, uniswap_v3::UniswapV3Pool};
use alloy::primitives::aliases::{I24, U24};
use alloy::primitives::{keccak256, Bytes, Signed, I256, U160};
use alloy::providers::{Network, Provider};
use alloy::rpc::types::{Filter, FilterSet};
use alloy::sol_types::{SolEvent, SolInterface, SolValue};
use alloy::{
    eips::BlockId,
    primitives::{Address, B256, U256},
    rpc::types::Log,
    sol,
};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rayon::iter::{IntoParallelIterator, ParallelDrainRange, ParallelIterator};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;
use uniswap_v3_math::tick_math::{MAX_TICK, MIN_TICK};

pub mod lense;

#[derive(Error, Debug)]
pub enum UniswapV4Error {
    #[error("Unknown Event Signature {0}")]
    UnknownEventSignature(B256),
    #[error("Not initialized")]
    NotInitialized,
}

sol! {
    #[sol(rpc)]
    interface IPoolManager {
        type PoolId is bytes32;
        type Currency is address;
        type BalanceDelta is int256;
        struct SwapParams {
            bool zeroForOne;
            int256 amountSpecified;
            uint160 sqrtPriceLimitX96;
        }
        #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        struct PoolKey {
            address currency0;
            address currency1;
            uint24 fee;
            int24 tickSpacing;
            address hooks;
        }
        event Initialize(
            PoolId indexed id,
            Currency indexed currency0,
            Currency indexed currency1,
            uint24 fee,
            int24 tickSpacing,
            IHooks hooks,
            uint160 sqrtPriceX96,
            int24 tick
        );
        event ModifyLiquidity(
            PoolId indexed id, address indexed sender, int24 tickLower, int24 tickUpper, int256 liquidityDelta, bytes32 salt
        );
        event Swap(
            PoolId indexed id,
            address indexed sender,
            int128 amount0,
            int128 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint24 fee
        );
        function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData)
            external
            returns (BalanceDelta swapDelta);

        /// @notice Called by external contracts to access granular pool state
        /// @param slot Key of slot to sload
        /// @return value The value of the slot as bytes32
        function extsload(bytes32 slot) external view returns (bytes32 value);
        function extsload(bytes32 startSlot, uint256 nSlots) external view returns (bytes32[] memory values);
        function extsload(bytes32[] calldata slots) external view returns (bytes32[] memory values);
    }

    interface IHooks {}
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniswapV4Pool {
    pub pool_key: IPoolManager::PoolKey,
    pub pool_id: B256,
    pub pool_data: UniswapV3Pool,
    pub hooks: Address,
    pub protocol_fee: u32,
    pub initialized: bool,
}

impl AutomatedMarketMaker for UniswapV4Pool {
    fn address(&self) -> Address {
        self.pool_data.address
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            IPoolManager::Initialize::SIGNATURE_HASH,
            IPoolManager::ModifyLiquidity::SIGNATURE_HASH,
            IPoolManager::Swap::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<(), AMMError> {
        let event_signature = log.topics()[0];

        match event_signature {
            IPoolManager::ModifyLiquidity::SIGNATURE_HASH => {
                let event = IPoolManager::ModifyLiquidity::decode_log(&log.inner)?;
                self.pool_data.modify_position(
                    event.tickLower.as_i32(),
                    event.tickUpper.as_i32(),
                    i128::from_be_bytes(
                        event
                            .liquidityDelta
                            .to_be_bytes::<32>()
                            .as_ref()
                            .try_into()
                            .unwrap(),
                    ),
                )?;
            }
            IPoolManager::Swap::SIGNATURE_HASH => {
                let event = IPoolManager::Swap::decode_log(&log.inner)?;
                self.pool_data.sqrt_price = U256::from(event.sqrtPriceX96);
                self.pool_data.tick = event.tick.as_i32();
                self.pool_data.liquidity = event.liquidity;
            }
            _ => {
                return Err(AMMError::UniswapV4Error(
                    UniswapV4Error::UnknownEventSignature(event_signature),
                ))
            }
        }

        Ok(())
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let pool = self.pool_data.clone();
        pool.simulate_swap(base_token, _quote_token, amount_in)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let pool = &mut self.pool_data;
        pool.simulate_swap(base_token, _quote_token, amount_in)
    }

    fn tokens(&self) -> Vec<Address> {
        self.pool_data.tokens()
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        self.pool_data.calculate_price(base_token, _quote_token)
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool = self.pool_data.init::<N, P>(block_number, provider).await?;
        self.pool_data = pool;
        self.initialized = true;
        self.pool_key = self.get_pool_key()?;
        self.pool_id = self.get_pool_id()?;

        Ok(self)
    }
}

impl UniswapV4Pool {
    // Create a new, unsynced UniswapV3 pool
    pub fn new(address: Address) -> Self {
        Self {
            pool_data: UniswapV3Pool::new(address),
            ..Default::default()
        }
    }

    pub fn get_pool_key(&self) -> Result<PoolKey, AMMError> {
        if !self.initialized {
            return Err(AMMError::UniswapV4Error(UniswapV4Error::NotInitialized));
        }

        Ok(PoolKey {
            currency0: self.pool_data.token_a.address,
            currency1: self.pool_data.token_b.address,
            fee: U24::from(self.pool_data.fee),
            tickSpacing: I24::try_from_be_slice(self.pool_data.tick_spacing.to_be_bytes().as_ref())
                .expect("4 bytes; qed"),
            hooks: self.hooks,
        })
    }

    pub fn get_pool_id(&self) -> Result<B256, AMMError> {
        Ok(keccak256(
            (
                self.pool_data.token_a.address,
                self.pool_data.token_b.address,
                U24::from(self.pool_data.fee),
                I24::try_from_be_slice(self.pool_data.tick_spacing.to_be_bytes().as_ref())
                    .expect("4 bytes; qed"),
                self.hooks,
            )
                .abi_encode(),
        ))
    }

    pub fn swap_calldata(
        &self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        hook_data: Bytes,
    ) -> Result<Bytes, AMMError> {
        Ok(IPoolManagerCalls::swap(swapCall {
            key: self.get_pool_key()?,
            params: IPoolManager::SwapParams {
                zeroForOne: zero_for_one,
                amountSpecified: amount_specified,
                sqrtPriceLimitX96: U160::from(sqrt_price_limit_x_96),
            },
            hookData: hook_data,
        })
        .abi_encode()
        .into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct UniswapV4Factory {
    pub address: Address,
    pub creation_block: u64,
}

impl UniswapV4Factory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        UniswapV4Factory {
            address,
            creation_block,
        }
    }

    pub async fn get_all_pools<N, P>(
        &self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let disc_filter = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()]);

        let sync_provider = provider.clone();
        let mut futures = FuturesUnordered::new();

        let sync_step = 100_000;
        let mut latest_block = self.creation_block;
        while latest_block < block_number.as_u64().unwrap_or_default() {
            let mut block_filter = disc_filter.clone();
            let from_block = latest_block;
            let to_block = (from_block + sync_step).min(block_number.as_u64().unwrap_or_default());

            block_filter = block_filter.from_block(from_block);
            block_filter = block_filter.to_block(to_block);

            let sync_provider = sync_provider.clone();

            futures.push(async move { sync_provider.get_logs(&block_filter).await });

            latest_block = to_block + 1;
        }

        let mut pools = vec![];
        while let Some(res) = futures.next().await {
            let logs = res?;

            for log in logs {
                pools.push(self.create_pool(log)?);
            }
        }

        Ok(pools)
    }

    pub async fn sync_slot_0<N, P>(
        pools: &mut [UniswapV4Pool],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let futures = FuturesUnordered::new();
        for pool in pools.iter() {
            let provider = provider.clone();
            let pool_id = pool.pool_id;
            futures.push(async move {
                let ipool_manager = IPoolManagerInstance::new(pool.address(), provider);

                let state_slot = lense::get_pool_state_slot(pool_id);
                let data = ipool_manager
                    .extsload_0(B256::from(state_slot))
                    .block(block_number)
                    .call()
                    .await?;

                let sqrt_price_x96 = U160::from_be_slice(&data[12..32]);

                let tick_bytes =
                    unsafe { (data.as_ptr().add(9) as *const [u8; 3]).read_unaligned() };
                let tick = I24::from_be_bytes(tick_bytes);

                let protocol_fee_bytes =
                    unsafe { (data.as_ptr().add(6) as *const [u8; 3]).read_unaligned() };
                let protocol_fee = U24::from_be_bytes(protocol_fee_bytes);

                let lp_fee_bytes =
                    unsafe { (data.as_ptr().add(3) as *const [u8; 3]).read_unaligned() };
                let lp_fee = U24::from_be_bytes(lp_fee_bytes);

                Ok((
                    U256::from(sqrt_price_x96),
                    tick.as_i32(),
                    protocol_fee.to::<u32>(),
                    lp_fee.to::<u32>(),
                ))
            })
        }

        let results = futures
            .collect::<Vec<Result<(U256, i32, u32, u32), AMMError>>>()
            .await;

        for (pool, res) in pools.iter_mut().zip(results.into_iter()) {
            let (sqrt_price_x96, tick, protocol_fee, lp_fee) = res?;
            pool.pool_data.sqrt_price = sqrt_price_x96;
            pool.pool_data.tick = tick;
            pool.pool_data.fee = lp_fee;
            pool.protocol_fee = protocol_fee;
        }

        Ok(())
    }

    pub async fn sync_token_decimals<N, P>(
        pools: &mut [UniswapV4Pool],
        provider: P,
    ) -> Result<(), BatchContractError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // Get all token decimals
        let mut tokens = HashSet::new();
        for pool in pools.iter() {
            for token in pool.tokens() {
                tokens.insert(token);
            }
        }
        let token_decimals = get_token_decimals(tokens.into_iter().collect(), provider).await?;

        // Set token decimals
        for pool in pools.iter_mut() {
            if let Some(decimals) = token_decimals.get(&pool.pool_data.token_a.address) {
                pool.pool_data.token_a.decimals = *decimals;
            }

            if let Some(decimals) = token_decimals.get(&pool.pool_data.token_b.address) {
                pool.pool_data.token_b.decimals = *decimals;
            }
        }

        Ok(())
    }

    pub async fn sync_tick_data<N, P>(
        pools: &mut [UniswapV4Pool],
        block_id: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool_ticks = pools
            .into_par_iter()
            .filter_map(|pool| {
                let min_word = tick_to_word(MIN_TICK, pool.pool_data.tick_spacing);
                let max_word = tick_to_word(MAX_TICK, pool.pool_data.tick_spacing);
                let pool_data = pool.pool_data.clone();
                let initialized_ticks: Vec<Signed<24, 1>> = (min_word..=max_word)
                    // Filter out empty bitmaps
                    .filter_map(|word_pos| {
                        pool_data
                            .tick_bitmap
                            .get(&(word_pos as i16))
                            .filter(|&bitmap| *bitmap != U256::ZERO)
                            .map(|&bitmap| (word_pos, bitmap))
                    })
                    // Get tick index for non zero bitmaps
                    .flat_map(|(word_pos, bitmap)| {
                        (0..256)
                            .filter(move |i| {
                                (bitmap & (U256::from(1) << U256::from(*i))) != U256::ZERO
                            })
                            .map(move |i| {
                                let tick_index = (word_pos * 256 + i) * pool_data.tick_spacing;

                                // TODO: update to use from be bytes or similar
                                Signed::<24, 1>::from_str(&tick_index.to_string()).unwrap()
                            })
                    })
                    .collect();

                // Only return pools with non-empty initialized ticks
                if !initialized_ticks.is_empty() {
                    Some((pool, initialized_ticks))
                } else {
                    None
                }
            })
            .collect::<Vec<(&mut UniswapV4Pool, Vec<Signed<24, 1>>)>>();

        let mut futures = FuturesUnordered::new();

        for (pool, ticks) in pool_ticks.into_iter() {
            let slots = ticks
                .iter()
                .map(|tick| get_tick_info_slot(pool.pool_id, tick.as_i32()))
                .collect::<Vec<U256>>();

            let provider = provider.clone();
            let pool_address = pool.address();
            futures.push(async move {
                let slots = slots
                    .chunks(500)
                    .map(|chunk| async {
                        let ipool_manager =
                            IPoolManagerInstance::new(pool_address, provider.clone());
                        let words = ipool_manager
                            .extsload_2(chunk.iter().cloned().map(B256::from).collect())
                            .block(block_id)
                            .call()
                            .await?;

                        Ok::<Vec<U256>, AMMError>(
                            words
                                .into_iter()
                                .map(|word| U256::from_be_bytes(word.0))
                                .collect(),
                        )
                    })
                    .collect::<Vec<_>>();

                let res: Vec<_> = futures::future::try_join_all(slots).await?;

                let mut tick_data = vec![];

                for (data, tick) in res.iter().zip(ticks.iter().cloned()) {
                    let (liquidity_gross, liquidity_net) =
                        decode_liquidity_gross_and_net(data[0].into());

                    tick_data.push((tick, (liquidity_gross, liquidity_net)))
                }

                Ok::<Vec<(Signed<24, 1>, (u128, i128))>, AMMError>(tick_data)
            });

            while let Some(res) = futures.next().await {
                let tick_data = res?;
                for (tick, (liquidity_gross, liquidity_net)) in tick_data.into_iter() {
                    let word_pos = tick.as_i32() / 256;
                    let bit_pos = tick.as_i32() & 0xff;
                    let pool_data = pool.pool_data.clone();
                    let word = pool_data
                        .tick_bitmap
                        .get(&(word_pos as i16))
                        .unwrap_or(&U256::ZERO);

                    pool.pool_data.ticks.insert(
                        tick.as_i32(),
                        Info {
                            liquidity_gross,
                            liquidity_net,
                            initialized: (word & (U256::from(1) << U256::from(bit_pos)))
                                != U256::ZERO,
                        },
                    );
                }
            }
        }

        Ok(())
    }

    pub async fn sync_tick_bitmap<N, P>(
        pools: &mut [UniswapV4Pool],
        block_id: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        for pool in pools.iter_mut() {
            let pool_data = pool.pool_data.clone();
            let pool_address = pool.address();
            let min_word = tick_to_word(MIN_TICK, pool_data.tick_spacing);
            let max_word = tick_to_word(MAX_TICK, pool_data.tick_spacing);

            let bitmap_slots = (min_word + 1..max_word)
                .filter_map(|word| {
                    let tick = word * pool_data.tick_spacing * 256;
                    if get_tick_bitmap_slot(pool.pool_id, tick) != U256::ZERO {
                        Some(get_tick_bitmap_slot(pool.pool_id, tick))
                    } else {
                        None
                    }
                })
                .collect::<Vec<U256>>();

            let provider = provider.clone();
            let futs = bitmap_slots.chunks(500).map(|chunk| {
                let provider = provider.clone();
                async move {
                    let words = IPoolManagerInstance::new(pool_address, provider.clone())
                        .extsload_2(chunk.iter().cloned().map(B256::from).collect())
                        .block(block_id)
                        .call()
                        .await?;

                    Ok::<Vec<U256>, AMMError>(
                        words
                            .into_iter()
                            .map(|word| U256::from_be_bytes(word.0))
                            .collect(),
                    )
                }
            });

            let res = futures::future::try_join_all(futs).await?;
            let bitmaps = res.into_iter().flatten().collect::<Vec<U256>>();

            for (i, word) in (min_word..=max_word).zip(bitmaps.into_iter()) {
                pool.pool_data.tick_bitmap.insert(i as i16, word);
            }
        }

        Ok(())
    }

    pub async fn sync_all_pools<N, P>(
        amms: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut pools: Vec<UniswapV4Pool> = amms
            .into_iter()
            .filter_map(|amm| {
                if let AMM::UniswapV4Pool(uv4_pool) = amm {
                    Some(uv4_pool)
                } else {
                    None
                }
            })
            .collect();

        Self::sync_slot_0(&mut pools, block_number, provider.clone()).await?;

        Self::sync_token_decimals(&mut pools, provider.clone()).await?;

        pools = pools
            .par_drain(..)
            .filter(|pool| {
                !(pool.pool_data.liquidity > 0
                    && pool.pool_data.token_a.decimals > 0
                    && pool.pool_data.token_b.decimals > 0)
            })
            .collect();

        Self::sync_tick_bitmap(&mut pools, block_number, provider.clone()).await?;
        Self::sync_tick_data(&mut pools, block_number, provider.clone()).await?;

        Ok(pools.into_iter().map(AMM::UniswapV4Pool).collect())
    }
}

impl AutomatedMarketMakerFactory for UniswapV4Factory {
    type PoolVariant = UniswapV4Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn pool_creation_event(&self) -> B256 {
        IPoolManager::Initialize::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let event = IPoolManager::Initialize::decode_log(&log.inner)?;
        let mut pool_data = UniswapV3Pool::new(log.address());

        pool_data.token_a.address = event.currency0;
        pool_data.token_b.address = event.currency1;
        pool_data.fee = event.fee.to::<u32>();
        pool_data.tick_spacing = event.tickSpacing.as_i32();
        pool_data.sqrt_price = U256::from(event.sqrtPriceX96);
        pool_data.tick = event.tick.as_i32();

        Ok(AMM::UniswapV4Pool(UniswapV4Pool {
            pool_key: PoolKey {
                currency0: event.currency0,
                currency1: event.currency1,
                fee: event.fee,
                tickSpacing: event.tickSpacing,
                hooks: event.hooks,
            },
            pool_id: event.id,
            pool_data: pool_data,
            hooks: event.hooks,
            protocol_fee: 0,
            initialized: true,
        }))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

impl DiscoverySync for UniswapV4Factory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(
            target = "amms::uniswap_v4::discover",
            address = ?self.address,
            "Discovering all pools"
        );

        self.get_all_pools(to_block, provider.clone())
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(
            target = "amms::uniswap_v4::sync",
            address = ?self.address,
            "Syncing all pools"
        );

        UniswapV4Factory::sync_all_pools(amms, to_block, provider)
    }
}
