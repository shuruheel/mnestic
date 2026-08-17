// Copyright 2022, The Cozo Project Authors.
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
// If a copy of the MPL was not distributed with this file,
// You can obtain one at https://mozilla.org/MPL/2.0/.

#include <iostream>
#include <memory>
#include "db.h"
// cxx generates this header under the package name (mnestic-rocks).
#include "mnestic-rocks/src/bridge/mod.rs.h"
#include "rocksdb/utilities/options_util.h"

Options default_db_options() {
    Options options = Options();
    options.bottommost_compression = kZSTD;
    options.compression = kLZ4Compression;
    options.level_compaction_dynamic_level_bytes = true;
    options.max_background_jobs = 6;
    options.bytes_per_sync = 1048576;
    options.compaction_pri = kMinOverlappingRatio;
    BlockBasedTableOptions table_options;
    table_options.block_size = 16 * 1024;
    table_options.cache_index_and_filter_blocks = true;
    table_options.pin_l0_filter_and_index_blocks_in_cache = true;
    table_options.format_version = 5;

    auto table_factory = NewBlockBasedTableFactory(table_options);
    options.table_factory.reset(table_factory);

    return options;
}

ColumnFamilyOptions default_cf_options() {
    ColumnFamilyOptions options = ColumnFamilyOptions();
    options.bottommost_compression = kZSTD;
    options.compression = kLZ4Compression;
    options.level_compaction_dynamic_level_bytes = true;
    options.compaction_pri = kMinOverlappingRatio;

//    auto cache = NewLRUCache(128 << 20);

    BlockBasedTableOptions table_options;
//    table_options.block_cache = cache;

    table_options.block_size = 16 * 1024;
    table_options.cache_index_and_filter_blocks = true;
    table_options.pin_l0_filter_and_index_blocks_in_cache = true;
    table_options.format_version = 5;

    auto table_factory = NewBlockBasedTableFactory(table_options);
    options.table_factory.reset(table_factory);

    return options;
}

// Shared body of `open_db` and `open_db_with_resources` (mnestic fork,
// docs/specs/cross-instance-memory.md §4/§7). `memory` may be null (the plain
// `open_db` path — behavior bit-identical to before the refactor); when
// present, its cache and WriteBufferManager are installed in both cache
// branches and any options-file `block_cache` is overridden, with the
// override recorded in `report` for the Rust side to log.
static shared_ptr<RocksDbBridge>
open_db_impl(const DbOpts &opts, const shared_ptr<RocksMemoryResources> &memory,
             RocksDbStatus &status, DbOpenReport &report) {
    auto options = default_db_options();

    const bool use_shared = memory != nullptr;
    shared_ptr<Cache> cache = nullptr;

    if (use_shared) {
        // The shared cache IS the envelope: it takes precedence over the
        // programmatic `block_cache_size` path and over any `block_cache` an
        // options file declares (override reported below).
        cache = memory->cache;
        report.used_shared_resources = true;
        report.shared_cache_capacity = memory->cache->GetCapacity();
    } else if (opts.block_cache_size > 0) {
        // Honour the size the caller asked for. This used to hardcode 1 GiB and ignore
        // `block_cache_size` entirely, so a caller requesting 64 MB silently got sixteen
        // times that.
        cache = NewLRUCache(opts.block_cache_size);
    }

    if (!opts.options_path.empty()) {
        DBOptions loaded_db_opt;
        std::vector<ColumnFamilyDescriptor> loaded_cf_descs;
        ConfigOptions config_options;
        string options_path = convert_vec_to_string(opts.options_path);
        Status s = LoadOptionsFromFile(config_options, options_path, &loaded_db_opt,
                                       &loaded_cf_descs);
        if (!s.ok()) {
            write_status(s, status);
            return nullptr;
        }
        if (loaded_cf_descs.empty()) {
            // Same defect class as the loop fix below: descriptor [0] is
            // consumed unconditionally, so an empty descriptor list must be a
            // loud error, not undefined behavior.
            write_status(Status::InvalidArgument("options file contains no column family"),
                         status);
            return nullptr;
        }

        if (cache != nullptr) {
            // (S13 fix) This loop used to index descriptor [0] on every
            // iteration and dereference `GetOptions<BlockBasedTableOptions>()`
            // without the null check its sibling fix block below has — a
            // non-block-based table factory in the options file crashed the
            // open. Only descriptor [0] survives into the single-CF
            // `TransactionDB::Open` below, but attach to every descriptor
            // honestly and skip the ones that cannot carry a cache.
            for (size_t i = 0; i < loaded_cf_descs.size(); ++i) {
                auto &cf_options = loaded_cf_descs[i].options;
                if (cf_options.table_factory == nullptr) {
                    continue;
                }
                auto *loaded_bbt_opt =
                        cf_options.table_factory->GetOptions<BlockBasedTableOptions>();
                if (loaded_bbt_opt == nullptr) {
                    continue;
                }
                if (use_shared && i == 0 && loaded_bbt_opt->block_cache != nullptr) {
                    // The options file declared its own cache; the shared
                    // envelope wins. Never silently: report both capacities.
                    report.overrode_options_file_cache = true;
                    report.options_file_cache_capacity =
                            loaded_bbt_opt->block_cache->GetCapacity();
                }
                loaded_bbt_opt->block_cache = cache;
                if (use_shared) {
                    loaded_bbt_opt->cache_index_and_filter_blocks_with_high_priority = true;
                }
            }
        }

        options = Options(loaded_db_opt, loaded_cf_descs[0].options);
    }

    if (opts.prepare_for_bulk_load) {
        options.PrepareForBulkLoad();
    }
    if (opts.increase_parallelism > 0) {
        options.IncreaseParallelism(opts.increase_parallelism);
    }
    if (opts.optimize_level_style_compaction) {
        options.OptimizeLevelStyleCompaction();
    }
    options.create_if_missing = opts.create_if_missing;
    options.paranoid_checks = opts.paranoid_checks;
    if (opts.enable_blob_files) {
        options.enable_blob_files = true;

        options.min_blob_size = opts.min_blob_size;

        options.blob_file_size = opts.blob_file_size;

        options.enable_blob_garbage_collection = opts.enable_blob_garbage_collection;
    }
    if (opts.use_bloom_filter || cache != nullptr) {
        // Start from the table options ALREADY IN EFFECT, never from a default-constructed
        // BlockBasedTableOptions.
        //
        // By this point `options.table_factory` carries everything an options file supplied
        // (block_cache, block_size, cache_index_and_filter_blocks, checksum, ...) plus the
        // defaults set in default_db_options(). Default-constructing here and then
        // reset()-ing the factory silently threw ALL of it away and kept only the two fields
        // assigned below — so a caller who configured a 128 MB block cache and a 16 KB block
        // size in an options file got RocksDB's 8 MB / 4 KB defaults instead, with no error
        // and no way to find out. `use_bloom_filter` is set unconditionally by
        // cozo-core's rocksdb backend, so this fired on every open.
        //
        // GetOptions<BlockBasedTableOptions>() is the same idiom already used above for the
        // block_cache_size path; it returns null if the factory is not block-based, in which
        // case a fresh default really is the right starting point.
        BlockBasedTableOptions table_options;
        if (options.table_factory != nullptr) {
            auto *existing = options.table_factory->GetOptions<BlockBasedTableOptions>();
            if (existing != nullptr) {
                table_options = *existing;
            }
        }
        if (opts.use_bloom_filter) {
            table_options.filter_policy.reset(
                    NewBloomFilterPolicy(opts.bloom_filter_bits_per_key, false));
            table_options.whole_key_filtering = opts.bloom_filter_whole_key_filtering;
        }
        if (cache != nullptr) {
            // Without this the cache built above was allocated and then dropped on the floor
            // whenever no options file was supplied.
            table_options.block_cache = cache;
        }
        if (use_shared) {
            // Index/filter blocks of participating instances go through the
            // shared cache's high-priority pool so full memtable pressure
            // (WBM dummy entries) evicts data blocks before metadata.
            table_options.cache_index_and_filter_blocks_with_high_priority = true;
        }
        options.table_factory.reset(NewBlockBasedTableFactory(table_options));
    }
    if (opts.use_capped_prefix_extractor) {
        options.prefix_extractor.reset(NewCappedPrefixTransform(opts.capped_prefix_extractor_len));
    }
    if (opts.use_fixed_prefix_extractor) {
        options.prefix_extractor.reset(NewFixedPrefixTransform(opts.fixed_prefix_extractor_len));
    }
    if (use_shared) {
        // One WriteBufferManager across every participating instance; its
        // budget is charged into the shared cache (cost-to-cache dummy
        // entries). Must be on DBOptions before TransactionDB::Open.
        options.write_buffer_manager = memory->write_buffer_manager;
    }
    options.create_missing_column_families = true;

    shared_ptr <RocksDbBridge> db = make_shared<RocksDbBridge>();

    db->db_path = convert_vec_to_string(opts.db_path);

    TransactionDB *txn_db = nullptr;
    write_status(
            TransactionDB::Open(options, TransactionDBOptions(), db->db_path, &txn_db),
            status);
    db->db.reset(txn_db);
    db->destroy_on_exit = opts.destroy_on_exit;


    return db;
}

shared_ptr <RocksDbBridge> open_db(const DbOpts &opts, RocksDbStatus &status) {
    // No memory handle: bit-identical to the pre-refactor body (the report is
    // written but discarded — nothing to override without a handle).
    DbOpenReport report{};
    return open_db_impl(opts, nullptr, status, report);
}

shared_ptr <RocksDbBridge>
open_db_with_resources(const DbOpts &opts, shared_ptr<RocksMemoryResources> resources,
                       RocksDbStatus &status, DbOpenReport &report) {
    report = DbOpenReport{};
    return open_db_impl(opts, resources, status, report);
}

shared_ptr <RocksMemoryResources> new_memory_resources(size_t total_bytes, double memtable_fraction) {
    return make_shared<RocksMemoryResources>(total_bytes, memtable_fraction);
}

RocksDbBridge::~RocksDbBridge() {
    if (destroy_on_exit && (db != nullptr)) {
        cerr << "destroying database on exit: " << db_path << endl;
        auto status = db->Close();
        if (!status.ok()) {
            cerr << status.ToString() << endl;
        }
        db.reset();
        Options options{};
        auto status2 = DestroyDB(db_path, options);
        if (!status2.ok()) {
            cerr << status2.ToString() << endl;
        }
    }
}
