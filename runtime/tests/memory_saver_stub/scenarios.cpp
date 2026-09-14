#include "utils.h"
#include <atomic>
#include <mutex>
#include <string>
#include <unordered_map>
// Test-only access for unreachable counter overflow and corrupt metadata.
// Production classes gain no test entrypoints or alternate allocator model.
#define private public
#include "core.h"
#undef private
#include "driver.h"
#include <cassert>
#include <cstring>
#include <iostream>
#include <thread>

// Independent ABI consumer. Weak linkage gives a behavioral missing-export RED.
struct Record {
    uint64_t address, size_bytes, backup_bytes;
    int32_t device;
    uint32_t state, backup_enabled, tag_length;
    char tag[64];
};
extern "C" uint32_t snapshot_probe(uint32_t, uint32_t, Record*, uint32_t, uint32_t*) noexcept
    asm("tms_snapshot_v1") __attribute__((weak));
static_assert(sizeof(Record) == 104);
int main(int argc, char** argv) {
    assert(argc == 2);
    assert(snapshot_probe && "missing real-source snapshot export");
    const std::string scenario = argv[1];
    Record records[4];
    uint32_t count = 99;
    auto snapshot = [&](uint32_t capacity = 4) {
        return snapshot_probe(1, sizeof(Record), records, capacity, &count);
    };
    if (scenario == "abi") {
        assert(snapshot() == 0 && count == 0);
        assert(snapshot(4096) == 0 && count == 0);
        for (uint32_t capacity : {0u, 4097u}) {
            count = 99;
            assert(snapshot(capacity) == 2 && count == 0);
        }
        count = 99;
        assert(snapshot_probe(2, 104, records, 4, &count) == 2 && count == 0);
        count = 99;
        assert(snapshot_probe(1, 103, records, 4, &count) == 2 && count == 0);
        count = 99;
        assert(snapshot_probe(1, 104, nullptr, 4, &count) == 2 && count == 0);
        assert(snapshot_probe(1, 104, records, 4, nullptr) == 2);
    } else if (scenario == "records") {
        auto& saver = TorchMemorySaver::instance();
        void *weights, *kv, *backup;
        saver.malloc(&weights, 0, 4096, "weights", false);
        saver.malloc(&kv, 1, 8192, "kv", false);
        saver.malloc(&backup, 0, 1024, "other", true);
        const auto calls = stub::calls();
        assert(snapshot() == 0 && count == 3);
        assert(stub::calls() == calls);
        for (uint32_t i = 0; i < count; ++i) {
            assert(records[i].state == 1 && records[i].backup_bytes == 0);
        }
        saver.pause("weights");
        saver.pause("other");
        assert(snapshot() == 0 && count == 3);
        for (uint32_t i = 0; i < count; ++i) {
            const auto& record = records[i];
            const std::string tag(record.tag, record.tag_length);
            if (tag == "weights") {
                assert(record.address == reinterpret_cast<uintptr_t>(weights));
                assert(record.size_bytes == 4096 && record.device == 0 && record.state == 2);
                assert(record.backup_enabled == 0 && record.backup_bytes == 0);
            } else if (tag == "kv") {
                assert(record.size_bytes == 8192 && record.device == 1 && record.state == 1);
                assert(record.backup_enabled == 0 && record.backup_bytes == 0);
            } else {
                assert(tag == "other" && record.size_bytes == 1024 && record.device == 0 && record.state == 2);
                assert(record.backup_enabled == 1 && record.backup_bytes == 1024);
            }
            for (unsigned j = record.tag_length; j < 64; ++j) assert(record.tag[j] == 0);
        }
        std::memset(records, 0x5a, sizeof(records));
        Record untouched[4]; std::memcpy(untouched, records, sizeof(records));
        assert(snapshot(2) == 3 && count == 0);
        assert(std::memcmp(records, untouched, sizeof(records)) == 0);
        saver.resume("weights"); saver.resume("other");
        assert(snapshot() == 0 && count == 3);
        for (uint32_t i = 0; i < count; ++i) {
            assert(records[i].state == 1);
            if (records[i].backup_enabled) assert(records[i].backup_bytes == 1024);
        }
        saver.free(weights); saver.free(kv); saver.free(backup);
        assert(snapshot() == 0 && count == 0);
    } else if (scenario == "maximum_capacity") {
        auto& saver = TorchMemorySaver::instance();
        void* pointer;
        for (unsigned i = 0; i < 4096; ++i) saver.malloc(&pointer, 0, 1, "", false);
        std::vector<Record> all(4096);
        assert(snapshot_probe(1, 104, all.data(), 4096, &count) == 0 && count == 4096);
        saver.malloc(&pointer, 1, 1, std::string(63, 'x'), false);
        std::memset(all.data(), 0x5a, all.size() * sizeof(Record));
        assert(snapshot_probe(1, 104, all.data(), 4096, &count) == 3 && count == 0);
        const auto* bytes = reinterpret_cast<const unsigned char*>(all.data());
        for (size_t i = 0; i < all.size() * sizeof(Record); ++i) assert(bytes[i] == 0x5a);
    } else if (scenario == "counter_overflow") {
        auto& saver = TorchMemorySaver::instance();
        saver.mutations_in_flight_ = UINT64_MAX;
        void* pointer;
        assert(stub_allocate(4096, 0, "weights", false, &pointer) == 0);
        assert(snapshot() == 2 && count == 0);
        saver.mutations_in_flight_ = 0;
        assert(snapshot() == 2 && count == 0);
    } else if (scenario == "fallback_error") {
        stub::fail("fallback_free", false);
        assert(stub_free(reinterpret_cast<void*>(0x5000)) == 1);
        assert(TorchMemorySaver::instance().mutations_in_flight_ == 0);
        assert(snapshot() == 2 && count == 0);
    } else if (scenario == "metadata_corruption") {
        auto& saver = TorchMemorySaver::instance();
        void* pointer;
        saver.malloc(&pointer, 0, 4096, "weights", false);
        auto& metadata = saver.allocation_metadata_.at(pointer);
        metadata.state = static_cast<AllocationState>(99);
        assert(snapshot() == 2 && count == 0);
        metadata.state = AllocationState::ACTIVE;
        metadata.cpu_backup = reinterpret_cast<void*>(0x2000);
        assert(snapshot() == 2 && count == 0);
        metadata.cpu_backup = nullptr;
        metadata.enable_cpu_backup = true;
        metadata.state = AllocationState::PAUSED;
        assert(snapshot() == 2 && count == 0);
        metadata.state = AllocationState::ACTIVE;
        metadata.size = UINT64_MAX;
        assert(snapshot() == 2 && count == 0);
        metadata.size = 4096;
        auto entry = saver.allocation_metadata_.extract(pointer);
        entry.key() = nullptr;
        saver.allocation_metadata_.insert(std::move(entry));
        assert(snapshot() == 2 && count == 0);
    } else if (scenario == "duplicate_insertion") {
        void *first, *second;
        assert(stub_allocate(4096, 0, "first", false, &first) == 0);
        stub::reuse_next_address(reinterpret_cast<uintptr_t>(first));
        assert(stub_allocate(8192, 0, "second", false, &second) == 0);
        assert(snapshot() == 2 && count == 0);
    } else if (scenario == "malloc_window" || scenario == "free_window") {
        void* pointer = nullptr;
        if (scenario == "free_window") assert(stub_allocate(4096, 0, "weights", false, &pointer) == 0);
        stub::arm(scenario == "malloc_window" ? "access" : "unmap");
        int result = -99;
        std::thread mutation([&] {
            result = scenario == "malloc_window" ? stub_allocate(4096, 0, "weights", false, &pointer) : stub_free(pointer);
        });
        stub::wait_entered();
        std::memset(records, 0x5a, sizeof(records));
        Record untouched[4]; std::memcpy(untouched, records, sizeof(records));
        const auto status = snapshot();
        const auto busy_count = count;
        stub::release();
        mutation.join();
        assert(status == 1 && "in-flight physical transition must be Busy");
        assert(busy_count == 0 && result == 0);
        assert(std::memcmp(records, untouched, sizeof(records)) == 0);
        assert(snapshot() == 0 && count == (scenario == "malloc_window" ? 1u : 0u));
    } else if (scenario == "overlap") {
        void *old_pointer, *new_pointer;
        assert(stub_allocate(2048, 0, "old", false, &old_pointer) == 0);
        stub::arm("access");
        std::thread allocating([&] { assert(stub_allocate(4096, 1, "new", false, &new_pointer) == 0); });
        stub::wait_entered();
        assert(stub_free(old_pointer) == 0);
        const auto status = snapshot();
        stub::release(); allocating.join();
        assert(status == 1 && "completed free must not clear overlapping malloc");
        assert(snapshot() == 0 && count == 1 && records[0].size_bytes == 4096);
    } else if (scenario == "pause_contention" || scenario == "resume_contention" || scenario == "fallback") {
        auto& saver = TorchMemorySaver::instance();
        void* pointer;
        if (scenario != "fallback") {
            saver.malloc(&pointer, 0, 4096, "weights", false);
            if (scenario == "resume_contention") saver.pause("weights");
        }
        stub::arm(scenario == "pause_contention" ? "unmap" : scenario == "resume_contention" ? "access" : "fallback_free");
        std::thread mutation([&] {
            if (scenario == "fallback") assert(stub_free(reinterpret_cast<void*>(0x5000)) == 0);
            else if (scenario == "pause_contention") saver.pause("weights");
            else saver.resume("weights");
        });
        stub::wait_entered();
        const auto status = snapshot();
        stub::release(); mutation.join();
        assert(status == 1 && count == 0);
        assert(snapshot() == 0 && count == (scenario == "fallback" ? 0u : 1u));
        if (scenario != "fallback") assert(records[0].state == (scenario == "pause_contention" ? 2u : 1u));
    } else if (scenario.rfind("exception_", 0) == 0 || scenario.rfind("fatal_", 0) == 0) {
        auto& saver = TorchMemorySaver::instance();
        const bool exception = scenario.rfind("exception_", 0) == 0;
        const auto action = scenario.substr(exception ? 10 : 6);
        void* pointer = nullptr;
        if (action != "malloc" && action != "fallback") saver.malloc(&pointer, 0, 4096, "weights", true);
        if (action == "resume") saver.pause("weights");
        stub::fail(action == "malloc" || action == "resume" ? "access" : action == "fallback" ? "fallback_free" : "release", exception);
        bool caught = false;
        try {
            if (action == "malloc") saver.malloc(&pointer, 0, 4096, "weights", false);
            else if (action == "free") saver.free(pointer);
            else if (action == "pause") saver.pause("weights");
            else if (action == "resume") saver.resume("weights");
            else saver.free(reinterpret_cast<void*>(0x5000));
        } catch (...) { caught = true; }
        if (!exception) {
            std::cout << "unexpected surviving observation" << std::endl;
            return 0;
        }
        assert(caught);
        assert(saver.mutations_in_flight_ == 0 && "guard must clean up after an exception");
        assert(snapshot() == 2 && count == 0 && "exception after physical effects must invalidate observation");
        // Surviving later mutations keep their upstream behavior; invalidity stays latched.
        void* other;
        saver.malloc(&other, 1, 128, "later", false);
        assert(snapshot() == 2 && count == 0);
    } else if (scenario.rfind("invalid_", 0) == 0) {
        void* pointer;
        std::string tag = "valid";
        if (scenario == "invalid_tag_length") tag = std::string(64, 'a');
        if (scenario == "invalid_tag_nul") tag = std::string("x\0y", 3);
        TorchMemorySaver::instance().malloc(&pointer, scenario == "invalid_device" ? -1 : 0,
                                           scenario == "invalid_size" ? 0 : 4096, tag, false);
        std::memset(records, 0x5a, sizeof(records));
        Record untouched[4]; std::memcpy(untouched, records, sizeof(records));
        assert(snapshot() == 2 && count == 0);
        assert(std::memcmp(records, untouched, sizeof(records)) == 0);
    } else {
        assert(false && "unknown scenario");
    }
}
