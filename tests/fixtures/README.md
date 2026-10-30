# ABI 测试夹具

`libevil.so` 是一个极小的 x86_64 ELF 共享对象，只为 ABI Guard 测试服务：

- 它的 `.gnu.version_r` 指向一个假的 `libc.so.6`；
- 符号版本字符串为 `GLIBC_2.00`；
- 测试会在内存中把等长的 `GLIBC_2.00` 替换成 `GLIBC_2.44` 或 `GLIBC_2.45`，
  然后写入临时文件，验证边界版本和超基线版本。
- 该文件不用于运行，只用于解析 `.gnu.version_r`。