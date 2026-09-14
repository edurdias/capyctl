#pragma once
#include <cstddef>
#include <cstdint>
using CUdevice = int;
using CUresult = int;
using CUdeviceptr = uintptr_t;
using CUmemGenericAllocationHandle = uint64_t;
constexpr int CUDA_SUCCESS = 0;
constexpr int CU_MEM_ALLOCATION_TYPE_PINNED = 1;
constexpr int CU_MEM_LOCATION_TYPE_DEVICE = 1;
constexpr int CU_MEM_ACCESS_FLAGS_PROT_READWRITE = 3;
constexpr int CU_DEVICE_ATTRIBUTE_GPU_DIRECT_RDMA_WITH_CUDA_VMM_SUPPORTED = 1;
struct CUmemLocation { int type; int id; };
struct CUmemAllocationProp {
    int type;
    CUmemLocation location;
    struct { unsigned char gpuDirectRDMACapable; } allocFlags;
};
struct CUmemAccessDesc { CUmemLocation location; unsigned long long flags; };
CUresult cuDeviceGetAttribute(int*, int, CUdevice);
CUresult cuMemCreate(CUmemGenericAllocationHandle*, size_t, const CUmemAllocationProp*, unsigned long long);
CUresult cuMemAddressReserve(CUdeviceptr*, size_t, size_t, CUdeviceptr, unsigned long long);
CUresult cuMemMap(CUdeviceptr, size_t, size_t, CUmemGenericAllocationHandle, unsigned long long);
CUresult cuMemSetAccess(CUdeviceptr, size_t, const CUmemAccessDesc*, size_t);
CUresult cuMemUnmap(CUdeviceptr, size_t);
CUresult cuMemRelease(CUmemGenericAllocationHandle);
CUresult cuMemAddressFree(CUdeviceptr, size_t);
CUresult cuGetErrorString(CUresult, const char**);
CUresult cuCtxGetDevice(CUdevice*);
CUresult cuDeviceGet(CUdevice*, int);
