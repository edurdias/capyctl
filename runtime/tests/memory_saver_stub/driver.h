#pragma once
#include <cstdint>
#include <string>
namespace stub {
// Gates stop inside the driver call: access follows mapping; unmap follows erase.
void arm(const std::string& operation);
void wait_entered();
void release();
void fail(const std::string& operation, bool exception);
uint64_t calls();
void reuse_next_address(uint64_t address);
}
extern "C" int stub_allocate(uint64_t size, int device, const char* tag, bool backup, void** pointer);
extern "C" int stub_free(void* pointer);
