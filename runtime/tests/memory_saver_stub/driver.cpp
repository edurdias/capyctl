// CPU control-flow fixture. No CUDA library is linked or called.
#include "core.h"
#include "api_forwarder.h"
#include "driver.h"
#include <atomic>
#include <condition_variable>
#include <stdexcept>

namespace {
std::atomic<uint64_t> call_count{0}, next_address{0x100000}, next_handle{1};
std::mutex gate_mutex;
std::condition_variable gate_condition;
std::string gated_operation, failed_operation;
bool entered = false, released = false, throws = false;
int operation(const char* name) {
    ++call_count;
    std::unique_lock<std::mutex> lock(gate_mutex);
    if (gated_operation == name) {
        entered = true;
        gate_condition.notify_all();
        gate_condition.wait(lock, [] { return released; });
    }
    if (failed_operation == name) {
        failed_operation.clear();
        if (throws) throw std::runtime_error("injected driver exception");
        return 1;
    }
    return 0;
}
}
namespace stub {
void arm(const std::string& name) {
    std::lock_guard<std::mutex> lock(gate_mutex);
    gated_operation = name; entered = false; released = false;
}
void wait_entered() {
    std::unique_lock<std::mutex> lock(gate_mutex);
    gate_condition.wait(lock, [] { return entered; });
}
void release() {
    std::lock_guard<std::mutex> lock(gate_mutex);
    released = true; gated_operation.clear(); gate_condition.notify_all();
}
void fail(const std::string& name, bool exception) {
    std::lock_guard<std::mutex> lock(gate_mutex);
    failed_operation = name; throws = exception;
}
uint64_t calls() { return call_count.load(); }
void reuse_next_address(uint64_t address) { next_address = address; }
}
CUresult cuDeviceGetAttribute(int* value, int, CUdevice) { *value = 0; return operation("attribute"); }
CUresult cuMemCreate(CUmemGenericAllocationHandle* handle, size_t, const CUmemAllocationProp*, unsigned long long) {
    *handle = next_handle++; return operation("create");
}
CUresult cuMemAddressReserve(CUdeviceptr* pointer, size_t size, size_t, CUdeviceptr, unsigned long long) {
    *pointer = next_address.fetch_add(size + 4096); return operation("reserve");
}
CUresult cuMemMap(CUdeviceptr, size_t, size_t, CUmemGenericAllocationHandle, unsigned long long) { return operation("map"); }
CUresult cuMemSetAccess(CUdeviceptr, size_t, const CUmemAccessDesc*, size_t) { return operation("access"); }
CUresult cuMemUnmap(CUdeviceptr, size_t) { return operation("unmap"); }
CUresult cuMemRelease(CUmemGenericAllocationHandle) { return operation("release"); }
CUresult cuMemAddressFree(CUdeviceptr, size_t) { return operation("address_free"); }
CUresult cuGetErrorString(CUresult, const char** message) { *message = "injected failure"; return 0; }
CUresult cuCtxGetDevice(CUdevice* device) { *device = 0; return operation("context"); }
CUresult cuDeviceGet(CUdevice* device, int ordinal) { *device = ordinal; return operation("device"); }
cudaError_t cudaMallocHost(void** pointer, size_t) { *pointer = reinterpret_cast<void*>(0x2000); return operation("host_alloc"); }
cudaError_t cudaFreeHost(void*) { return operation("host_free"); }
cudaError_t cudaMemcpy(void*, const void*, size_t, int) { return operation("copy"); }
const char* cudaGetErrorString(cudaError_t) { return "injected failure"; }
namespace APIForwarder {
cudaError_t call_real_cuda_malloc(void** pointer, size_t) {
    *pointer = reinterpret_cast<void*>(0x3000); return operation("fallback_malloc");
}
cudaError_t call_real_cuda_free(void*) { return operation("fallback_free"); }
}
extern "C" int stub_allocate(uint64_t size, int device, const char* tag, bool backup, void** pointer) {
    try { return TorchMemorySaver::instance().malloc(pointer, device, size, tag, backup); }
    catch (...) { return -1; }
}
extern "C" int stub_free(void* pointer) {
    try { return TorchMemorySaver::instance().free(pointer); }
    catch (...) { return -1; }
}
