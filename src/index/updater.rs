use {
  self::inscription_updater::InscriptionUpdater,
  super::{tx_fetcher::TxFetcher, *},
  futures::future::try_join_all,
  std::sync::mpsc,
  tokio::sync::mpsc::{error::TryRecvError, Receiver, Sender},
};

mod inscription_updater;

struct BlockData {
  header: BlockHeader,
  txdata: Vec<(Transaction, Txid)>,
}

impl From<Block> for BlockData {
  fn from(block: Block) -> Self {
    BlockData {
      header: block.header,
      txdata: block
        .txdata
        .into_iter()
        .map(|transaction| {
          let txid = transaction.txid();
          (transaction, txid)
        })
        .collect(),
    }
  }
}

pub(crate) struct Updater {
  range_cache: HashMap<OutPointValue, Vec<u8>>,
  height: u64,
  index_sats: bool,
  sat_ranges_since_flush: u64,
  outputs_cached: u64,
  outputs_inserted_since_flush: u64,
  outputs_traversed: u64,
}

impl Updater {
  pub(crate) fn update(index: &Index) -> Result {
    let wtx = index.begin_write()?;

    let height = wtx
      .open_table(HEIGHT_TO_BLOCK_HASH)?
      .range(0..)?
      .rev()
      .next()
      .map(|(height, _hash)| height.value() + 1)
      .unwrap_or(0);

    wtx
      .open_table(WRITE_TRANSACTION_STARTING_BLOCK_COUNT_TO_TIMESTAMP)?
      .insert(
        &height,
        &SystemTime::now()
          .duration_since(SystemTime::UNIX_EPOCH)
          .map(|duration| duration.as_millis())
          .unwrap_or(0),
      )?;

    let mut updater = Self {
      range_cache: HashMap::new(),
      height,
      index_sats: index.has_sat_index()?,
      sat_ranges_since_flush: 0,
      outputs_cached: 0,
      outputs_inserted_since_flush: 0,
      outputs_traversed: 0,
    };

    updater.update_index(index, wtx)
  }

  fn update_index<'index>(
    &mut self,
    index: &'index Index,
    mut wtx: WriteTransaction<'index>,
  ) -> Result {
    let starting_height = index.client.get_block_count()? + 1;

    let mut progress_bar = if cfg!(test)
      || log_enabled!(log::Level::Info)
      || starting_height <= self.height
      || integration_test()
    {
      None
    } else {
      let progress_bar = ProgressBar::new(starting_height);
      progress_bar.set_position(self.height);
      progress_bar.set_style(
        ProgressStyle::with_template("[indexing blocks] {wide_bar} {pos}/{len}").unwrap(),
      );
      Some(progress_bar)
    };

    let rx = Self::fetch_blocks_from(index, self.height, self.index_sats)?;

    let (mut outpoint_sender, mut value_receiver) = Self::spawn_tx_fetcher(index)?;

    let mut uncommitted = 0;
    let mut value_cache = HashMap::new();
    loop {
      let block = match rx.recv() {
        Ok(block) => block,
        Err(mpsc::RecvError) => break,
      };

      self.index_block(
        index,
        &mut outpoint_sender,
        &mut value_receiver,
        &mut wtx,
        block,
        &mut value_cache,
      )?;

      if let Some(progress_bar) = &mut progress_bar {
        progress_bar.inc(1);

        if progress_bar.position() > progress_bar.length().unwrap() {
          progress_bar.set_length(index.client.get_block_count()? + 1);
        }
      }

      uncommitted += 1;

      if uncommitted == 5000 {
        self.commit(wtx, value_cache)?;
        value_cache = HashMap::new();
        uncommitted = 0;
        wtx = index.begin_write()?;
        let height = wtx
          .open_table(HEIGHT_TO_BLOCK_HASH)?
          .range(0..)?
          .rev()
          .next()
          .map(|(height, _hash)| height.value() + 1)
          .unwrap_or(0);
        if height != self.height {
          // another update has run between committing and beginning the new
          // write transaction
          break;
        }
        wtx
          .open_table(WRITE_TRANSACTION_STARTING_BLOCK_COUNT_TO_TIMESTAMP)?
          .insert(
            &self.height,
            &SystemTime::now()
              .duration_since(SystemTime::UNIX_EPOCH)
              .map(|duration| duration.as_millis())
              .unwrap_or(0),
          )?;
      }

      if INTERRUPTS.load(atomic::Ordering::Relaxed) > 0 {
        break;
      }
    }

    if uncommitted > 0 {
      self.commit(wtx, value_cache)?;
    }

    if let Some(progress_bar) = &mut progress_bar {
      progress_bar.finish_and_clear();
    }

    Ok(())
  }

  fn fetch_blocks_from(
    index: &Index,
    mut height: u64,
    index_sats: bool,
  ) -> Result<mpsc::Receiver<BlockData>> {
    let (tx, rx) = mpsc::sync_channel(32);

    let height_limit = index.height_limit;

    let client =
      Client::new(&index.rpc_url, index.auth.clone()).context("failed to connect to RPC URL")?;

    let first_inscription_height = index.first_inscription_height;

    thread::spawn(move || loop {
      if let Some(height_limit) = height_limit {
        if height >= height_limit {
          break;
        }
      }

      match Self::get_block_with_retries(&client, height, index_sats, first_inscription_height) {
        Ok(Some(block)) => {
          if let Err(err) = tx.send(block.into()) {
            log::info!("Block receiver disconnected: {err}");
            break;
          }
          height += 1;
        }
        Ok(None) => break,
        Err(err) => {
          log::error!("failed to fetch block {height}: {err}");
          break;
        }
      }
    });

    Ok(rx)
  }

  fn get_block_with_retries(
    client: &Client,
    height: u64,
    index_sats: bool,
    first_inscription_height: u64,
  ) -> Result<Option<Block>> {
    let mut errors = 0;
    loop {
      match client
        .get_block_hash(height)
        .into_option()
        .and_then(|option| {
          option
            .map(|hash| {
              if index_sats || height >= first_inscription_height {
                Ok(client.get_block(&hash)?)
              } else {
                Ok(Block {
                  header: client.get_block_header(&hash)?,
                  txdata: Vec::new(),
                })
              }
            })
            .transpose()
        }) {
        Err(err) => {
          if cfg!(test) {
            return Err(err);
          }

          errors += 1;
          let seconds = 1 << errors;
          log::warn!("failed to fetch block {height}, retrying in {seconds}s: {err}");

          if seconds > 120 {
            log::error!("would sleep for more than 120s, giving up");
            return Err(err);
          }

          thread::sleep(Duration::from_secs(seconds));
        }
        Ok(result) => return Ok(result),
      }
    }
  }

  fn spawn_tx_fetcher(index: &Index) -> Result<(Sender<OutPoint>, Receiver<u64>)> {
    let tx_fetcher = TxFetcher::new(&index.rpc_url, index.auth.clone())?;

    const CHANNEL_BUFFER_SIZE: usize = 20_000;
    let (outpoint_sender, mut outpoint_receiver) =
      tokio::sync::mpsc::channel::<OutPoint>(CHANNEL_BUFFER_SIZE);
    let (value_sender, value_receiver) = tokio::sync::mpsc::channel::<u64>(CHANNEL_BUFFER_SIZE);

    const BATCH_SIZE: usize = 4096;
    const RPC_THREADS: usize = 4;

    std::thread::spawn(move || {
      let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
      rt.block_on(async move {
        loop {
          let Some(outpoint) = outpoint_receiver.recv().await else {
            log::info!("Outpoint channel closed");
            return;
          };
          let mut outpoints = vec![outpoint];
          for _ in 0..BATCH_SIZE-1 {
            let Ok(outpoint) = outpoint_receiver.try_recv() else {
              break;
            };
            outpoints.push(outpoint);
          }
          let parts = (outpoints.len() / RPC_THREADS).max(1);
          let mut futs = Vec::with_capacity(RPC_THREADS);
          for chunk in outpoints.chunks(parts) {
            let txids = chunk.iter().map(|outpoint| outpoint.txid).collect();
            let fut = tx_fetcher.get_transactions(txids);
            futs.push(fut);
          }
          let txs = match try_join_all(futs).await {
            Ok(txs) => txs,
            Err(e) => {
            log::warn!("Couldn't receive txs {e}");
            return;
            }
          };
          for (i, tx) in txs.iter().flatten().enumerate() {
            let Ok(_) = value_sender.send(tx.output[usize::try_from(outpoints[i].vout).unwrap()].value).await else {
              log::warn!("Value channel closed unexpectedly");
              return;
            };
          }
        }
      })
    });

    Ok((outpoint_sender, value_receiver))
  }

  fn index_block(
    &mut self,
    index: &Index,
    outpoint_sender: &mut Sender<OutPoint>,
    value_receiver: &mut Receiver<u64>,
    wtx: &mut WriteTransaction,
    block: BlockData,
    value_cache: &mut HashMap<OutPoint, u64>,
  ) -> Result<()> {
    let Err(TryRecvError::Empty) = value_receiver.try_recv() else { return Err(anyhow!("Previous block did not consume all input values")); };

    let mut outpoint_to_value = wtx.open_table(OUTPOINT_TO_VALUE)?;

    if !self.index_sats {
      let txids = block
        .txdata
        .iter()
        .map(|(_, txid)| txid)
        .collect::<HashSet<_>>();
      for (tx, _) in &block.txdata {
        for input in &tx.input {
          let prev_output = input.previous_output;
          if prev_output.is_null() {
            continue;
          }
          if txids.contains(&prev_output.txid) {
            continue;
          }
          if value_cache.contains_key(&prev_output) {
            continue;
          }
          if outpoint_to_value.get(&prev_output.store())?.is_some() {
            continue;
          }
          outpoint_sender.blocking_send(prev_output)?;
        }
      }
    }

    let mut height_to_block_hash = wtx.open_table(HEIGHT_TO_BLOCK_HASH)?;

    let start = Instant::now();
    let mut sat_ranges_written = 0;
    let mut outputs_in_block = 0;

    let time = timestamp(block.header.time);

    log::info!(
      "Block {} at {} with {} transactions…",
      self.height,
      time,
      block.txdata.len()
    );

    if let Some(prev_height) = self.height.checked_sub(1) {
      let prev_hash = height_to_block_hash.get(&prev_height)?.unwrap();

      if prev_hash.value() != block.header.prev_blockhash.as_ref() {
        index.reorged.store(true, atomic::Ordering::Relaxed);
        return Err(anyhow!("reorg detected at or before {prev_height}"));
      }
    }

    let mut inscription_id_to_inscription_entry =
      wtx.open_table(INSCRIPTION_ID_TO_INSCRIPTION_ENTRY)?;
    let mut inscription_id_to_satpoint = wtx.open_table(INSCRIPTION_ID_TO_SATPOINT)?;
    let mut inscription_number_to_inscription_id =
      wtx.open_table(INSCRIPTION_NUMBER_TO_INSCRIPTION_ID)?;
    let mut sat_to_inscription_id = wtx.open_table(SAT_TO_INSCRIPTION_ID)?;
    let mut satpoint_to_inscription_id = wtx.open_table(SATPOINT_TO_INSCRIPTION_ID)?;
    let mut statistic_to_count = wtx.open_table(STATISTIC_TO_COUNT)?;

    let mut lost_sats = statistic_to_count
      .get(&Statistic::LostSats.key())?
      .map(|lost_sats| lost_sats.value())
      .unwrap_or(0);

    let mut inscription_updater = InscriptionUpdater::new(
      self.height,
      &mut inscription_id_to_satpoint,
      value_receiver,
      &mut inscription_id_to_inscription_entry,
      lost_sats,
      &mut inscription_number_to_inscription_id,
      &mut outpoint_to_value,
      &mut sat_to_inscription_id,
      &mut satpoint_to_inscription_id,
      block.header.time,
      value_cache,
    )?;

    if self.index_sats {
      let mut sat_to_satpoint = wtx.open_table(SAT_TO_SATPOINT)?;
      let mut outpoint_to_sat_ranges = wtx.open_table(OUTPOINT_TO_SAT_RANGES)?;

      let mut coinbase_inputs = VecDeque::new();

      let h = Height(self.height);
      if h.subsidy() > 0 {
        let start = h.starting_sat();
        coinbase_inputs.push_front((start.n(), (start + h.subsidy()).n()));
        self.sat_ranges_since_flush += 1;
      }

      for (tx_offset, (tx, txid)) in block.txdata.iter().enumerate().skip(1) {
        log::trace!("Indexing transaction {tx_offset}…");

        let mut input_sat_ranges = VecDeque::new();

        for input in &tx.input {
          let key = input.previous_output.store();

          let sat_ranges = match self.range_cache.remove(&key) {
            Some(sat_ranges) => {
              self.outputs_cached += 1;
              sat_ranges
            }
            None => outpoint_to_sat_ranges
              .remove(&key)?
              .ok_or_else(|| anyhow!("Could not find outpoint {} in index", input.previous_output))?
              .value()
              .to_vec(),
          };

          for chunk in sat_ranges.chunks_exact(11) {
            input_sat_ranges.push_back(SatRange::load(chunk.try_into().unwrap()));
          }
        }

        self.index_transaction_sats(
          tx,
          *txid,
          &mut sat_to_satpoint,
          &mut input_sat_ranges,
          &mut sat_ranges_written,
          &mut outputs_in_block,
          &mut inscription_updater,
        )?;

        coinbase_inputs.extend(input_sat_ranges);
      }

      if let Some((tx, txid)) = block.txdata.get(0) {
        self.index_transaction_sats(
          tx,
          *txid,
          &mut sat_to_satpoint,
          &mut coinbase_inputs,
          &mut sat_ranges_written,
          &mut outputs_in_block,
          &mut inscription_updater,
        )?;
      }

      if !coinbase_inputs.is_empty() {
        let mut lost_sat_ranges = outpoint_to_sat_ranges
          .remove(&OutPoint::null().store())?
          .map(|ranges| ranges.value().to_vec())
          .unwrap_or_default();

        for (start, end) in coinbase_inputs {
          if !Sat(start).is_common() {
            sat_to_satpoint.insert(
              &start,
              &SatPoint {
                outpoint: OutPoint::null(),
                offset: lost_sats,
              }
              .store(),
            )?;
          }

          lost_sat_ranges.extend_from_slice(&(start, end).store());

          lost_sats += end - start;
        }

        outpoint_to_sat_ranges.insert(&OutPoint::null().store(), lost_sat_ranges.as_slice())?;
      }
    } else {
      for (tx, txid) in block.txdata.iter().skip(1).chain(block.txdata.first()) {
        lost_sats += inscription_updater.index_transaction_inscriptions(tx, *txid, None)?;
      }
    }

    statistic_to_count.insert(&Statistic::LostSats.key(), &lost_sats)?;

    height_to_block_hash.insert(&self.height, &block.header.block_hash().store())?;

    self.height += 1;
    self.outputs_traversed += outputs_in_block;

    log::info!(
      "Wrote {sat_ranges_written} sat ranges from {outputs_in_block} outputs in {} ms",
      (Instant::now() - start).as_millis(),
    );

    Ok(())
  }

  fn index_transaction_sats(
    &mut self,
    tx: &Transaction,
    txid: Txid,
    sat_to_satpoint: &mut Table<u64, &SatPointValue>,
    input_sat_ranges: &mut VecDeque<(u64, u64)>,
    sat_ranges_written: &mut u64,
    outputs_traversed: &mut u64,
    inscription_updater: &mut InscriptionUpdater,
  ) -> Result {
    inscription_updater.index_transaction_inscriptions(tx, txid, Some(input_sat_ranges))?;

    for (vout, output) in tx.output.iter().enumerate() {
      let outpoint = OutPoint {
        vout: vout.try_into().unwrap(),
        txid,
      };
      let mut sats = Vec::new();

      let mut remaining = output.value;
      while remaining > 0 {
        let range = input_sat_ranges
          .pop_front()
          .ok_or_else(|| anyhow!("insufficient inputs for transaction outputs"))?;

        if !Sat(range.0).is_common() {
          sat_to_satpoint.insert(
            &range.0,
            &SatPoint {
              outpoint,
              offset: output.value - remaining,
            }
            .store(),
          )?;
        }

        let count = range.1 - range.0;

        let assigned = if count > remaining {
          self.sat_ranges_since_flush += 1;
          let middle = range.0 + remaining;
          input_sat_ranges.push_front((middle, range.1));
          (range.0, middle)
        } else {
          range
        };

        sats.extend_from_slice(&assigned.store());

        remaining -= assigned.1 - assigned.0;

        *sat_ranges_written += 1;
      }

      *outputs_traversed += 1;

      self.range_cache.insert(outpoint.store(), sats);
      self.outputs_inserted_since_flush += 1;
    }

    Ok(())
  }

  fn commit(&mut self, wtx: WriteTransaction, value_cache: HashMap<OutPoint, u64>) -> Result {
    log::info!(
      "Committing at block height {}, {} outputs traversed, {} in map, {} cached",
      self.height,
      self.outputs_traversed,
      self.range_cache.len(),
      self.outputs_cached
    );

    if self.index_sats {
      log::info!(
        "Flushing {} entries ({:.1}% resulting from {} insertions) from memory to database",
        self.range_cache.len(),
        self.range_cache.len() as f64 / self.outputs_inserted_since_flush as f64 * 100.,
        self.outputs_inserted_since_flush,
      );

      let mut outpoint_to_sat_ranges = wtx.open_table(OUTPOINT_TO_SAT_RANGES)?;

      for (outpoint, sat_range) in self.range_cache.drain() {
        outpoint_to_sat_ranges.insert(&outpoint, sat_range.as_slice())?;
      }

      self.outputs_inserted_since_flush = 0;
    }

    {
      let mut outpoint_to_value = wtx.open_table(OUTPOINT_TO_VALUE)?;

      for (outpoint, value) in value_cache {
        outpoint_to_value.insert(&outpoint.store(), &value)?;
      }
    }

    Index::increment_statistic(&wtx, Statistic::OutputsTraversed, self.outputs_traversed)?;
    self.outputs_traversed = 0;
    Index::increment_statistic(&wtx, Statistic::SatRanges, self.sat_ranges_since_flush)?;
    self.sat_ranges_since_flush = 0;
    Index::increment_statistic(&wtx, Statistic::Commits, 1)?;

    wtx.commit()?;
    Ok(())
  }
}
