#pragma once
#include <cstddef>
using cudaError_t = int;
using cudaStream_t = void*;
constexpr int cudaSuccess = 0;
constexpr int cudaMemcpyDeviceToHost = 1;
constexpr int cudaMemcpyHostToDevice = 2;
cudaError_t cudaMallocHost(void**, size_t);
cudaError_t cudaFreeHost(void*);
cudaError_t cudaMemcpy(void*, const void*, size_t, int);
const char* cudaGetErrorString(cudaError_t);
