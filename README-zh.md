# mnestic

> mnestic 是 [CozoDB](https://github.com/cozodb/cozo) 的一个独立维护的分支（fork），专注于成为**智能体记忆（agentic memory）**的底层引擎。mnestic **不是**官方 CozoDB，也未获其原作者背书或与之关联。原始设计的全部功劳归于 Ziyang Hu 与 Cozo Project Authors。详见 [`FORK.md`](FORK.md) 与 [`CHANGELOG-FORK.md`](CHANGELOG-FORK.md)。以下为保留的上游 CozoDB 文档。

---

<img src="static/logo_c.png" width="200" height="175" alt="Logo">

[![Crates.io](https://img.shields.io/crates/v/mnestic)](https://crates.io/crates/mnestic) [![PyPI](https://img.shields.io/pypi/v/mnestic)](https://pypi.org/project/mnestic/)

# Cozo 数据库

## mnestic 相比 CozoDB 新增的能力

上游最后一次提交停留在 2024-12-04。mnestic 在其之上继续维护引擎，并新增以下能力：

- **图投影缓存** —— `::graph create G { edges: knows }` 命名一份常驻内存的邻接结构，
  十二个图算法可跨查询复用，而不必每次调用都重新扫描边关系并重建 CSR。**始终新鲜**：
  投影绝不会返回与消费事务自身扫描不一致的数据；对源关系的写入会立即释放由它构建的邻接结构。
  （[规格说明](docs/specs/graph-projection.md)）
- **预算加权遍历** —— `BudgetedTraversal` 从一组种子节点出发，按非负权重做
  最廉价优先扩展，受**全局去重节点预算**约束（另有可选的代价上限与精确跳数上限），
  并支持扩展过程中的准入门控，为每个被采纳的节点输出 `(cost, parent, depth)`。
  构造即确定性、可中断，并可消费图投影缓存 —— 这是“用搜索命中周围最廉价的图邻域
  填满固定上下文窗口”所需的原语。
  （[规格说明](docs/specs/budgeted-traversal.md)）
- **双时态（bitemporal）** —— `TxTime` 列类型与崩溃安全的单调提交时钟、`:as_of` 读取、
  两级 `(有效时间, 事务时间)` 解析，以及 `::history` / `::history_gc` / `::evict`。
  （[规格说明](docs/specs/bitemporality.md)）
- **日历感知的日期时间库** —— 分量提取函数（`dt_year` … `dt_dow`）、`dt_trunc`、
  日历感知的 `dt_add` / `dt_diff`、带时区的 `dt_format`，以及 `dt_to_validity` ——
  从浮点 Unix**秒**到 `Validity` 整数微秒的有类型桥接，使时间戳以 validity 的身份
  抵达有效时间轴，而不再是一个引擎必须猜测其单位的裸数字。
- **来源半环（provenance semirings）** —— 递归中用户自定义的吸收性合并
  （`Db::register_custom_aggr`）、返回前 *k* 条推导及其证据链的 `min_cost_k` 有界 meet 聚合，
  以及基于重算的信念修订 `:reconcile`。
  （[规格说明](docs/specs/provenance-semirings.md)）
- **天际线 / Pareto 前沿聚合** —— `pareto_min` / `pareto_max` 按组保留数值向量上的
  非支配集（原生逐分量支配），从而呈现一个*争议集*（contested set）—— 若干个彼此都不占优的
  答案 —— 而非坍缩为单一赢家。可从任意绑定通过普通的 `run_script` 调用；任意调用方自定义的
  支配关系则可在 Rust 中经 `register_bounded_meet_aggr` 使用。
  （[规格说明](docs/specs/antichain-bounded-meet.md)）
- **引擎内混合检索** —— 以单个可被 Datalog 组合的 fixed rule，对向量、全文与图三路结果做
  RRF 倒数排名融合，并支持 MMR 多样化。每一路都可选，且图路可作为预算化的最廉价优先扩展运行
  —— 以向量/全文命中作为种子 —— 用检索命中周围最廉价的图邻域填满固定的上下文预算。
- **只读 Cypher** —— 将 openCypher 子集翻译为 CozoScript（alpha；`cypher` feature，默认关闭）。
  （[规格说明](docs/specs/cypher-read.md)）
- **更快的查找与计划** —— 等值下推把后置过滤的点查转为按键定位（5k 行实测约 28×），
  另有确定性贪心连接重排序与可选的因子化 `count()` 重写。
- **非阻塞的向量索引构建** —— HNSW 改为内存中并行构建，不再让读操作阻塞数分钟；
  搜索路径上的邻居向量通过 RocksDB `MultiGet` 批量获取。
- **可运维的损坏恢复** —— `::reindex` 依据数据库中已存储的索引配置，就地重建关系的
  HNSW / FTS / LSH 索引：批量导入与备份恢复都不会维护这些索引，而现在它们的修复路径
  不再是“先删除、再手工复原创建脚本”；`::repair_corrupt` 精确删除被截断的元组，
  而不必丢弃整个未通过完整性检查的数据库。
- **真正生效的可中断性** —— `::kill` 与 `:timeout` 能中断正在运行的查询，
  包括耗时较长的图邻接结构构建。

其余部分 —— CozoScript、存储引擎、数据模型 —— 均为上游 CozoDB，除非
[`CHANGELOG-FORK.md`](CHANGELOG-FORK.md) 中另有说明。

## 0.13.0 新增

一次正确性与能力并重的发布：一个九项缺陷的正确性合集（FTS 打分、HNSW/FTS 索引维护、
损坏值 blob 处理、恢复/打开时的关系 id 校正），外加一批新特性 —— 日期时间标准库、
`HybridSearch` 的预算化扩展模式，以及恢复的 `!=` 因子化计数重写。这是一次 minor
而非 patch：若干修复会改变结果（BM25 打分、图路融合排名），且 RocksDB 表选项修复会改变
*未来*写入的落盘块格式；但公开的 Rust API 除下述标注处外保持源码兼容。

**RocksDB 表选项现在会被正确采用（随 `mnestic-rocks` 0.1.10 一同发布）。** 你配置的
每一项 `BlockBasedTableOptions` —— 块缓存、块大小、索引/过滤器缓存 —— 在每次打开时
都被静默丢弃，于是无论你的 options 文件如何设置，引擎都以 8 MB 的默认缓存与 4 KB 的块
运行。**本次发布之前，任何针对 RocksDB 存储所测得的读路径基准，测的都是一个比 mnestic
实际更慢的引擎。** 已修复；新写入的 SST 会采用所配置的 `block_size`，既有 SST 仍可读取，
无需迁移。

**日期时间标准库（`dt_*`）。** 分量提取函数（`dt_year` … `dt_dow`）、`dt_trunc`、
日历感知的 `dt_add` / `dt_diff`、strftime 风格的 `dt_format`，以及双时态数据库所急需的
`dt_to_validity` —— 从浮点 Unix**秒**到 `Validity` 整数微秒的有类型桥接。`@` 与 `:as_of`
现在都接受 `Validity` 类型的表达式（`@ dt_to_validity(parse_timestamp('2024-01-01'))`），
与 0.12.2 对浮点数的拒绝一并，彻底堵住了「秒 vs 微秒」的陷阱。新的 `dt_*` 名称已被保留，
不可再用于自定义函数注册。

**`HybridSearch`：预算化图扩展与可选路。** 设置 `GraphLeg::max_nodes` 会把某一图路切换为
在去重节点预算下的最廉价优先加权扩展 —— 即 0.12.0 的 `BudgetedTraversal`，如今可从单次
调用的 `hybrid_search` 表面直接使用 —— 而 `vector_index` / `fts_index` 现在为 `Option`，
因此你可以融合 {向量, 全文, 图} 各路的任意非空子集。**破坏性变更（API）：** `HybridSearch`
与 `GraphLeg` 现为 `#[non_exhaustive]` —— 请以 `Default` 构造再设置字段 —— 这一代价在同一次
（新增了八个 `GraphLeg` 字段的）发布中一次付清，使今后新增字段不再造成破坏。递归模式的图路
不受影响；Python 字典表面只新增可选键。

**`!=` 因子化计数重写已恢复，置于类型门控之后，默认关闭。** 其容斥扩展（在 0.10.5 因
Int/Float 误计而被撤下）这次是可靠的：仅当每个不等式的两个操作数都是已声明、非空、
变体稳定的存储列时，重写才会触发。在 LSQB q6（sf0.1，SQLite）上实测：**41.7 s → 0.30 s
（约 140×）**，无论开关如何，计数都与官方 oracle 一致。以 `Db::set_query_factorization(true)`
启用；默认开启的切换需等待一次夜间浸泡测试。

**面向查询作者的更好报错。** 解析失败时，光标会指向解析器所到达的最深位置，并附上一行
`help:`，列出该处本可接受的字面记号（形如 `expected one of: :=, <-, <~`）；索引搜索诊断
现在会带上真正失败的那类索引的错误码（`fts_query_required` 而非 `hnsw_query_required` 等）。

**正确性合集 —— 你可能需要执行一次 `::reindex`。** BM25 的文档计数 `N` 现在直接从 FTS 索引
免费读取，而不再在每次查询时重新扫描整个基础关系（此前是 30× 的打分误差与随规模增长的
O(corpus) 延迟，二者均已删除）；空操作的重复 `:put` 不再抬高 FTS 文档计数；损坏的值 blob
与损坏的 HNSW/FTS 索引行现在是普通的查询错误，而非进程 panic；恢复/打开会校正关系 id 计数器，
不再静默复用一个仍在使用的 id。经由任一 ≤ 0.12.2 版本构建的 HNSW 索引，可能因 null 向量
与「在已有行上创建索引」等缺陷而残留过期的节点或边 —— 请对每个受影响的关系执行一次
`::reindex <relation>`（你的行数据不受影响，它只重建索引关系）。

完整细节 —— 含 HNSW / 损坏 blob / 恢复的逐项升级步骤 —— 见
[`CHANGELOG-FORK.md`](CHANGELOG-FORK.md)。


## 简介

[ 中文文档 | [English](./README.md) ]

Cozo是一个事务型关系型数据库：

* 一个 **可嵌入** 的数据库；
* 一个使用 **Datalog** 作为查询语句的数据库；
* 一个专注于 **图数据、图算法** 的数据库；
* 一个可进行 **历史穿梭** 查询的数据库；
* 一个支持 **高性能、高并发** 的数据库。

### “可嵌入”是什么意思？

如果某个数据库能在不联网的手机上使用，那它大概就是嵌入式的。举例来说，SQLite 是嵌入式的，而 MySQL、Postgres、Oracle 等不是（它们是客户端—服务器（CS）架构的数据库）。

> 如果数据库与你的主程序在同一进程中运行，那么它就是 _嵌入式_ 数据库。与此相对，在使用 _客户端—服务器_ 架构的数据库时，主程序需要通过特定的接口（通常是网络接口）访问数据库，而数据库也可能运行在另一台机器或独立的集群上。嵌入式数据库使用简单，资源占用少，并可以在更广泛的环境中使用。
>
> Cozo 同时也支持以客户端—服务器模式运行。因此，Cozo 是一个 _可嵌入_ 而不是仅仅是 _嵌入式_ 的数据库。在客户端—服务器模式下，Cozo 可以更充分地发挥服务器的性能。

### “图数据”有什么用？

从本质上来说，数据一定是相互关联、自关联的，而这种关联的数学表达便是 _图_ （也叫 _网络_）。只有考虑这些关联，才能更深入地洞察数据背后的逻辑。

> 大多数现有的 _图数据库_ 强制要求按照属性图（property graph）的范式存储数据。与此相对，Cozo 使用传统的关系数据模型。关系数据模型有存储逻辑简单、功能强劲等优点，并且处理图数据也毫无问题。更重要的是，数据的洞察常常需要挖掘隐含的关联，而关系数据模型作为关系 _代数_（relational algebra）可以很好地处理此类问题。比较而言，因为其不构成一个代数，属性图模型仅仅能够将显性的图关系作为图数据处理，可组合性很弱。

### “Datalog”好在哪儿？

Datalog 1977 年便出现了，它可表达所有的 _关系型查询_，而它与 SQL 比起来的优势在于其对 _递归_ 的表达。由于执行逻辑不同，Datalog 对于递归的运行，通常比相应的 SQL 查询更快。Datalog 的可组合性、模块性都很优秀，使用它，你可以逐层、清晰地表达所需的查询。

> 递归对于图查询尤其重要。Cozo 使用的 Datalog 方言 叫做 CozoScript，其允许在一定条件下混合使用聚合查询与递归，从而进一步增强了 Datalog 的表达能力。同时，Cozo内置了图分析中常用的一些算法（如 PageRank 等），调用简单。
>
> 对 Datalog 有进一步了解以后，你会发现 Datalog 的 _规则_ 类似于编程语言中的函数。规则的一大特点是其可组合性：将一个查询分解为多个渐进的规则可使查询更清晰、易维护，且不会有效率上的损失。与此相对的，复杂的 SQL 查询语句通常表达为多层嵌套的“select-from-where”，可读性、可维护性都不高。

### 历史穿梭？

在数据库中，“历史穿梭”的意思是记录数据的一切变化，以允许针对某一时刻的数据进行执行查询，用来窥探历史。

> 在某种意义上，这使数据库成为 _不可变_ 数据库，因为没有数据会被真正删除。
> 
> 每一项额外的功能都有其代价。如果不使用某个功能，理想的状态是不必为这个功能的代价埋单。在 Cozo 中，不是所有数据表都自动支持历史穿梭，这就把是否需要此功能、是否愿意支付代价的选择权交到了用户手里。
> 
> [这个](https://docs.cozodb.org/zh_CN/latest/releases/v0.4.html)关于历史穿梭的小故事可能启发出一些历史穿梭的应用场景。


### “高性能、高并发”，有多高？

我们在一台 2020 年的 Mac Mini 上，使用 RocksDB 持久性存储引擎（Cozo 支持多种存储引擎）做了性能测试：

* 对一个有 160 万行的表进行查询：读、写、改的混合事务性查询可达到每秒 10 万次，而只读查询可达到每秒 25 万次。在此过程中，数据库使用的内存峰值仅为50MB。
* 备份数据的速度为每秒约 100 万行，恢复速度为每秒约 40 万行。备份、恢复的速度不随表单数据增长而变慢。
* 分析查询：扫描一个有 160 万行的表大约需要 1 秒（根据具体查询语句大约有上下 2 倍以内的差异）。查询所需时间与查询所涉及的行数大致成比例，而内存使用主要决定于返回集合的大小。
* 对于一个有 160 万个顶点，3100 万条边的图数据表，“两跳”图查询（如查询某人的朋友的朋友都有谁）可在 1 毫秒内完成。
* Pagerank 算法速度：1 万个顶点，12 万条边：50 毫秒以内；10 个万顶点，170 万条边：1 秒以内；160 万个顶点，3100 万条边：30秒以内。

更多的细节参见[此文](https://docs.cozodb.org/zh_CN/latest/releases/v0.3.html)。

## 学习 Cozo

你得先安装一个数据库才能开始学，对吧？不一定：Cozo 是“嵌入式”的，所以我们直接把它通过 WASM 嵌入到浏览器里了！打开[这个页面](https://www.cozodb.org/wasm-demo/)，然后：

* [Cozo 入门教程](https://docs.cozodb.org/zh_CN/latest/tutorial.html)

当然也可以一步到位：先翻到后面了解如何在熟悉的环境里安装原生 Cozo 数据库，再开始学习。

### 一些示例

通过以下示例，可在正式开始学习之前对 Cozo 的查询先有个感性认识。

假设有个表，名为 `*route`，含有两列，名为 `fr` 和 `to`，其中数据为机场代码（如 `FRA` 是法兰克福机场的代码），且每行数据表示一个飞行航线。

从 `FRA` 可以不转机到达多少个机场：
```
?[count_unique(to)] := *route{fr: 'FRA', to}
```

| count_unique(to) |
|------------------|
| 310              |

从 `FRA` 出发，转机一次，可以到达多少个机场：
```
?[count_unique(to)] := *route{fr: 'FRA', to: stop},
                       *route{fr: stop, to}
```

| count_unique(to) |
|------------------|
| 2222             |

从 `FRA` 出发，转机任意次，可以到达多少个机场：
```
reachable[to] := *route{fr: 'FRA', to}
reachable[to] := reachable[stop], *route{fr: stop, to}
?[count_unique(to)] := reachable[to]
```

| count_unique(to) |
|------------------|
| 3462             |

从 `FRA` 出发，按所需的最少转机次数排序，到达哪两个机场需要最多的转机次数：
```
shortest_paths[to, shortest(path)] := *route{fr: 'FRA', to},
                                      path = ['FRA', to]
shortest_paths[to, shortest(path)] := shortest_paths[stop, prev_path],
                                      *route{fr: stop, to},
                                      path = append(prev_path, to)
?[to, path, p_len] := shortest_paths[to, path], p_len = length(path)

:order -p_len
:limit 2
```

| to  | path                                              | p_len |
|-----|---------------------------------------------------|-------|
| YPO | `["FRA","YYZ","YTS","YMO","YFA","ZKE","YAT","YPO"]` | 8     |
| BVI | `["FRA","AUH","BNE","ISA","BQL","BEU","BVI"]`        | 7     |

`FRA` 和 `YPO` 这两个机场之间最短的路径以及其实际飞行里程是多少：
```
start[] <- [['FRA']]
end[] <- [['YPO]]
?[src, dst, distance, path] <~ ShortestPathDijkstra(*route[], start[], end[])
```

| src | dst | distance | path                                                   |
|-----|-----|----------|--------------------------------------------------------|
| FRA | YPO | 4544.0   | `["FRA","YUL","YVO","YKQ","YMO","YFA","ZKE","YAT","YPO"]` |

当查询语句有错时，Cozo 会提供明确有用的错误信息：
```
?[x, Y] := x = 1, y = x + 1
```

<pre><span style="color: rgb(204, 0, 0);">eval::unbound_symb_in_head</span><span>

  </span><span style="color: rgb(204, 0, 0);">×</span><span> Symbol 'Y' in rule head is unbound
   ╭────
 </span><span style="color: rgba(0, 0, 0, 0.5);">1</span><span> │ ?[x, Y] := x = 1, y = x + 1
   · </span><span style="font-weight: bold; color: rgb(255, 0, 255);">     ─</span><span>
   ╰────
</span><span style="color: rgb(0, 153, 255);">  help: </span><span>Note that symbols occurring only in negated positions are not considered bound
</span></pre>

## 安装 Cozo

建议先学习，再安装。当然反过来我们也不反对。

Cozo 可以安装在一大堆不同的语言与环境中：

> **mnestic** 目前仅发布 Rust crate（[crates.io/mnestic](https://crates.io/crates/mnestic)）和一个 Python 绑定（[PyPI `mnestic`](https://pypi.org/project/mnestic/) — `pip install mnestic`）。下表是上游 CozoDB 的绑定矩阵，此处予以保留仅供参考。

| 语言/环境                                                                                                 | 官方支持的平台                                                                                              | 存储引擎  |
|-------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------|-------|
| [Python](https://github.com/cozodb/pycozo)（[国内镜像](https://gitee.com/cozodb/pycozo)）                   | Linux (x86_64), Mac (ARM64, x86_64), Windows (x86_64)                                                | MQR   |
| [NodeJS](./cozo-lib-nodejs)                                                                           | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                         | MQR   |
| [浏览器](./cozo-lib-wasm)                                                                                | 支持[WASM](https://developer.mozilla.org/en-US/docs/WebAssembly#browser_compatibility)的浏览器（较新的浏览器全都支持） | M     |
| [Java (JVM)](https://github.com/cozodb/cozo-lib-java)（[国内镜像](https://gitee.com/cozodb/cozo-lib-java)） | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                         | MQR   |
| [Clojure (JVM)](https://github.com/cozodb/cozo-clj)（[国内镜像](https://gitee.com/cozodb/cozo-clj)）        | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                         | MQR   |
| [安卓](https://github.com/cozodb/cozo-lib-android)（[国内镜像](https://gitee.com/cozodb/cozo-lib-android)）   | Android (ARM64, ARMv7, x86_64, x86)                                                                  | MQ    |
| [iOS/macOS (Swift)](./cozo-lib-swift)                                                                 | iOS (ARM64, 模拟器), Mac (ARM64, x86_64)                                                                | MQ    |
| [Rust](https://docs.rs/cozo/)                                                                         | 任何支持`std`的[平台](https://doc.rust-lang.org/nightly/rustc/platform-support.html)（源代码编译）                 | MQRST |
| [Go](https://github.com/cozodb/cozo-lib-go)（[国内镜像](https://gitee.com/cozodb/cozo-lib-go)）             | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                         | MQR   |
| [C/C++/支持 C FFI 的语言](./cozo-lib-c)                                                                    | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                         | MQR   |
| [独立的 HTTP 服务](./cozo-bin)                                                                             | Linux (x86_64, ARM64), Mac (ARM64, x86_64), Windows (x86_64)                                         | MQRST |

“存储引擎”列中各个字母的含义：

* M: 基于内存的非持久性存储引擎
* Q: 基于 [SQLite](https://www.sqlite.org/) 的存储引擎
* R: 基于 [RocksDB](http://rocksdb.org/) 的存储引擎
* S: 基于 [Sled](https://github.com/spacejam/sled) 的存储引擎
* T: 基于 [TiKV](https://tikv.org/) 的分布式存储引擎

Cozo 的 [Rust API 文档](https://docs.rs/cozo/)（英文）中有一些额外的关于存储选择的建议。

你也可以尝试为其它平台、语言、引擎自行编译 Cozo。可能需要调整一些代码，但总体来说不难。

### 优化基于 RocksDB 的存储引擎

RocksDB 有五花八门的选项以供用户进行性能调优。但是调优这个问题太复杂了，就连 RocksDB 他们自己也搞不定，所以实际生产中他们用的是强化学习来自动调优。对于 95% 的用户来说，费这个劲根本不值得，尤其是 Cozo “开箱”的设置就已经相当快、足够快了。

如果你坚信你是剩下那 5% 里面的：当你用 RocksDB 引擎创建 CozoDB 实例时，你提供过一个存储数据的目录路径。如果在这个目录里创建一个名为`options`的文件，RocksDB 引擎便会将其解读为 [RocksDB 选项文件](https://github.com/facebook/rocksdb/wiki/RocksDB-Options-File)
并应用其中的设置。如果使用的是独立的 `cozo` 程序，激活此功能时会有一条提示日志。

每次 RocksDB 引擎启动时，存储目录下的 `data/OPTIONS-XXXXXX` 文件会记录当前应用设置。你可以把这个文件拷贝出来，在其基础上修改。如果你不是 RocksDB 的专家，建议只改动那些你大概知道什么意思的数字型选项。设置不当可能会搞乱、搞坏数据库。

## Cozo 的架构

Cozo 数据库有三个上下游部分组成，其中每部分只调用下游部分的接口。

<table>
<tbody>
<tr><td>(<i>用户代码</i>)</td></tr>
<tr><td>语言/环境包装</td></tr>
<tr><td>查询引擎</td></tr>
<tr><td>存储引擎</td></tr>
<tr><td>(<i>操作系统</i>)</td></tr>
</tbody>
</table>

### 存储引擎

在存储引擎这一部分里，Cozo 定义了一个存储接口（Rust 中的 `trait`），这个接口的功能是对二进制数据的键值进行存储及范围扫描。目前这个接口有以下官方实现：

* 基于内存的非持久性存储引擎
* 基于 [SQLite](https://www.sqlite.org/) 的存储引擎
* 基于 [RocksDB](http://rocksdb.org/) 的存储引擎
* 基于 [Sled](https://github.com/spacejam/sled) 的存储引擎
* 基于 [TiKV](https://tikv.org/) 的分布式存储引擎

不是所有的二进制包都包含以上所有引擎。这些引擎中，SQLite 引擎具有特殊地位：Cozo 使用它的文件作为备份文件，用以在不同引擎的 Cozo 之间交换数据。Rust 使用者可以轻松实现自己的引擎（不是说写一个引擎很轻松，这里意思是把现有的引擎接入到 Cozo 里很轻松）。

Cozo 使用 _面向行_ 而非 _面向列_ 的二进制存储格式。在这个格式中，对键的存储通过 [memcomparable](https://github.com/facebook/mysql-5.6/wiki/MyRocks-record-format#memcomparable-format) 的方法将复合键存储为一个字节数组，而直接对这些字节数组按照字节顺序排序就能得到正确的语义排序。这也意味着直接用 SQL 查询在 SQLite 引擎中存储的数据得到的结果看起来像是乱码。实现存储引擎本身的接口并不需要了解这个格式。

### 查询引擎

查询引擎部分实现了以下功能：

* 各种函数、聚合算子、算法的实现
* 表单数据结构的定义（schema）
* 数据库查询事务（transaction）
* 查询语句的编译
* 查询的执行

这部分包含 Cozo 项目的大部分代码。关于查询的执行，文档中[有一整章](https://docs.cozodb.org/zh_CN/latest/execution.html)来详细介绍。

Cozo 的 [Rust API](https://docs.rs/cozo/) 实际上就是查询引擎的公共接口。

### 语言、环境封装

Cozo 的 Rust 以外的所有语言、环境都只是对 Rust API 的进一步封装。例如，在独立服务器（cozo）中，Rust API 被封装为了 HTTP 端点，而在 Cozo-Node 中，同步的Rust API 被封装为基于 JavaScript 运行时的异步 API。

封装 Rust API 不难，如果你想让 Cozo 在其它语言上跑起来可以试试。Rust 有一些现成的库用来与其它语言交互。如果你想用某个语言而没有现成的交互库，我们建议你直接封装 Cozo 的 C 语言 API。官方支持的 Go 库就是这么实现的（通过 cgo）。

## 项目进程

Cozo 一开始预想的功能已经实现得不少了，但是项目仍然年轻得很。欢迎各界朋友使用并提出宝贵意见。

Cozo 1.0 之前的版本不承诺语法、API 的稳定性或存储兼容性。

**实验性（alpha）**：默认关闭的 `cypher` feature 提供**只读 Cypher 查询面**，将 openCypher 子集翻译为 CozoScript（`DbInstance::run_cypher` / `cypher_to_script`，Python `run_cypher`）；Datalog 仍是原生的全功能查询语言。设计与限制见 [`docs/specs/cypher-read.md`](docs/specs/cypher-read.md)。

## 链接

* [项目主页](https://cozodb.org/)
* [文档](https://docs.cozodb.org/en/latest/)
* [主仓库](https://github.com/cozodb/cozo)
* [Rust 文档](https://docs.rs/cozo/)
* [问题追踪](https://github.com/cozodb/cozo/issues)
* [项目讨论](https://github.com/cozodb/cozo/discussions)
* [用户 Reddit](https://www.reddit.com/r/cozodb/)

## 许可证和贡献

Cozo 以 MPL-2.0 或其更高版本授权。如果你有兴趣为该项目贡献代码，请看[这里](CONTRIBUTING.md)。