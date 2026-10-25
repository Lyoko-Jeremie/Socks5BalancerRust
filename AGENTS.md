# AGENTS.md

***Follow Content Must Read With UTF-8 Encoding***

## 项目说明
这是一个 Rust 项目


## 开发指南

当前环境是 PowerShell 。

当前操作系统上有 ripgrep (rg) ， 可以使用 `rg` 命令进行文件内容搜索。

在完成所有修改后使用 `cargo clippy` 确认一切正常。

优先考虑是否有现成的库。

## 注意事项

此项目的所有源码，没有特殊要求的一律使用 UTF-8 编码。

### 性能问题
注意优化频繁调用的函数和被频繁执行的代码的性能问题：
1. 采用性能更好的算法
2. 采用更成熟的库

### 跨平台
要支持 Windows 和 Linux 运行

## 代码规范
注释以中文编写

