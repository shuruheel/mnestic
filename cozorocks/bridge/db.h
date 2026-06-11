// Copyright 2022, The Cozo Project Authors.
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
// If a copy of the MPL was not distributed with this file,
// You can obtain one at https://mozilla.org/MPL/2.0/.

#ifndef COZOROCKS_DB_H
#define COZOROCKS_DB_H

#include <utility>

#include "iostream"
#include "common.h"
#include "tx.h"
#include "slice.h"

struct SnapshotBridge {
    const Snapshot *snapshot;
    DB *db;

    explicit SnapshotBridge(const Snapshot *snapshot_, DB *db_) : snapshot(snapshot_), db(db_) {}

    ~SnapshotBridge() {
        db->ReleaseSnapshot(snapshot);
//        printf("released snapshot\n");
    }
};

// Snapshot-pinned read surface (mnestic fork): read-only scripts read the base
// DB through a plain snapshot instead of constructing a pessimistic
// Transaction (no lock manager, no write-batch overlay on every read). The
// snapshot is released when the bridge is dropped; iterators created from it
// must not outlive it (same discipline the Transaction-backed iterators
// already require).
struct SnapshotReadBridge {
    DB *db;
    const Snapshot *snapshot;
    unique_ptr<ReadOptions> r_opts;
    // multi_get scratch: results stay pinned here between `multi_get` and the
    // per-index `multi_get_val` reads, so values cross the FFI zero-copy.
    mutable std::vector<PinnableSlice> mg_vals;
    mutable std::vector<Status> mg_statuses;

    explicit SnapshotReadBridge(DB *db_)
            : db(db_), snapshot(db_->GetSnapshot()), r_opts(new ReadOptions) {
        r_opts->ignore_range_deletions = true;
        r_opts->snapshot = snapshot;
    }

    ~SnapshotReadBridge() {
        db->ReleaseSnapshot(snapshot);
    }

    inline unique_ptr<PinnableSlice> get(RustBytes key, RocksDbStatus &status) const {
        auto ret = make_unique<PinnableSlice>();
        auto s = db->Get(*r_opts, db->DefaultColumnFamily(), convert_slice(key), &*ret);
        write_status(s, status);
        return ret;
    }

    inline void exists(RustBytes key, RocksDbStatus &status) const {
        auto ret = PinnableSlice();
        auto s = db->Get(*r_opts, db->DefaultColumnFamily(), convert_slice(key), &ret);
        write_status(s, status);
    }

    inline unique_ptr<IterBridge> iterator() const {
        auto it = make_unique<IterBridge>(nullptr);
        it->db = db;
        it->set_snapshot(snapshot);
        return it;
    }

    // Keys arrive concatenated (`keys_concat`) with per-key lengths
    // (`key_lens`); one RocksDB MultiGet serves them all (shared bloom-filter
    // probes and batched block reads). Values are read back per index via
    // `multi_get_val`.
    inline void multi_get(RustBytes keys_concat, rust::Slice<const ::std::uint64_t> key_lens,
                          RocksDbStatus &status) const {
        const size_t n = key_lens.size();
        std::vector<Slice> keys;
        keys.reserve(n);
        const char *base = reinterpret_cast<const char *>(keys_concat.data());
        size_t off = 0;
        for (size_t i = 0; i < n; ++i) {
            keys.emplace_back(base + off, key_lens[i]);
            off += key_lens[i];
        }
        mg_vals = std::vector<PinnableSlice>(n);
        mg_statuses = std::vector<Status>(n);
        db->MultiGet(*r_opts, db->DefaultColumnFamily(), n, keys.data(), mg_vals.data(),
                     mg_statuses.data());
        write_status(Status::OK(), status);
    }

    inline RustBytes multi_get_val(::std::uint64_t i, RocksDbStatus &status) const {
        write_status(mg_statuses[i], status);
        return convert_pinnable_slice_back(mg_vals[i]);
    }
};

struct SstFileWriterBridge {
    SstFileWriter inner;

    SstFileWriterBridge(EnvOptions eopts, Options opts) : inner(eopts, opts) {
    }

    inline void finish(RocksDbStatus &status) {
        write_status(inner.Finish(), status);
    }

    inline void put(RustBytes key, RustBytes val, RocksDbStatus &status) {
        write_status(inner.Put(convert_slice(key), convert_slice(val)), status);
    }

};

static WriteOptions DEFAULT_WRITE_OPTIONS = WriteOptions();

struct RocksDbBridge {
    unique_ptr<TransactionDB> db;

    bool destroy_on_exit;
    string db_path;

    inline unique_ptr<SstFileWriterBridge> get_sst_writer(rust::Str path, RocksDbStatus &status) const {
        DB *db_ = get_base_db();
        auto cf = db->DefaultColumnFamily();
        Options options_ = db_->GetOptions(cf);
        auto sst_file_writer = std::make_unique<SstFileWriterBridge>(EnvOptions(), options_);
        string path_(path);

        write_status(sst_file_writer->inner.Open(path_), status);
        return sst_file_writer;
    }

    inline void ingest_sst(rust::Str path, RocksDbStatus &status) const {
        IngestExternalFileOptions ifo;
        DB *db_ = get_base_db();
        string path_(path);
        auto cf = db->DefaultColumnFamily();
        write_status(db_->IngestExternalFile(cf, {std::move(path_)}, ifo), status);
    }

    [[nodiscard]] inline const string &get_db_path() const {
        return db_path;
    }


    [[nodiscard]] inline unique_ptr<TxBridge> transact() const {
        auto ret = make_unique<TxBridge>(&*this->db, db->DefaultColumnFamily());
        return ret;
    }

    [[nodiscard]] inline unique_ptr<SnapshotReadBridge> snapshot_read() const {
        return make_unique<SnapshotReadBridge>(get_base_db());
    }

    inline void del_range(RustBytes start, RustBytes end, RocksDbStatus &status) const {
        WriteBatch batch;
        auto cf = db->DefaultColumnFamily();
        auto s = batch.DeleteRange(cf, convert_slice(start), convert_slice(end));
        if (!s.ok()) {
            write_status(s, status);
            return;
        }
        WriteOptions w_opts;
        TransactionDBWriteOptimizations optimizations;
        optimizations.skip_concurrency_control = true;
        optimizations.skip_duplicate_key_check = true;
        auto s2 = db->Write(w_opts, optimizations, &batch);
        write_status(s2, status);
    }

    inline void put(RustBytes key, RustBytes val, RocksDbStatus &status) const {
        auto raw_db = this->get_base_db();
        auto s = raw_db->Put(DEFAULT_WRITE_OPTIONS, convert_slice(key), convert_slice(val));
        write_status(s, status);
    }

    void compact_range(RustBytes start, RustBytes end, RocksDbStatus &status) const {
        CompactRangeOptions options;
        auto cf = db->DefaultColumnFamily();
        auto start_s = convert_slice(start);
        auto end_s = convert_slice(end);
        auto s = db->CompactRange(options, cf, &start_s, &end_s);
        write_status(s, status);
    }

    DB *get_base_db() const {
        return db->GetBaseDB();
    }

    ~RocksDbBridge();
};

shared_ptr<RocksDbBridge>
open_db(const DbOpts &opts, RocksDbStatus &status);

#endif //COZOROCKS_DB_H
