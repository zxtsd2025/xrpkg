//! 手写玩具求解器，保留作为 resolvo 的对照基线；主流程不调用本模块的 Solver。
//!
//! 求解器只负责挑选版本和生成安装计划，不处理安装、ABI、数据库等提交阶段工作。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::capability::{Capability, Version};

/// 仓库中的一个包版本及其 Capability 要求。
#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub version: Version,
    pub requires: Vec<Capability>,
}

impl Package {
    /// 用包名、已解析的版本和 Capability 要求列表构造一个包版本。
    pub fn new(name: impl Into<String>, version: Version, requires: Vec<Capability>) -> Self {
        Self {
            name: name.into(),
            version,
            requires,
        }
    }
}

/// 求解结果中的一个条目，名称与版本一一对应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedPackage {
    pub name: String,
    pub version: Version,
}

/// 一次成功求解的安装计划；条目按“被依赖者在前”排列。
#[derive(Clone, Debug, Default)]
pub struct Resolution {
    packages: Vec<SelectedPackage>,
}

impl Resolution {
    /// 按安装顺序返回选中的包。
    pub fn packages(&self) -> &[SelectedPackage] {
        &self.packages
    }

    /// 查询计划中某个包最终选中的版本。
    pub fn version_of(&self, name: &str) -> Option<&Version> {
        self.packages
            .iter()
            .find(|package| package.name == name)
            .map(|package| &package.version)
    }

    /// 根据“包名 -> 包索引”的选中集合生成安装计划。
    ///
    /// 该函数只负责拓扑排序，不关心版本是手写求解器还是 resolvo 选出来的。
    pub fn from_selected(
        packages: &[Package],
        root: &str,
        selected: &BTreeMap<String, usize>,
    ) -> Self {
        fn visit(
            packages: &[Package],
            name: &str,
            selected: &BTreeMap<String, usize>,
            visited: &mut BTreeSet<String>,
            ordered: &mut Vec<SelectedPackage>,
        ) {
            if !visited.insert(name.to_string()) {
                return;
            }

            let Some(&index) = selected.get(name) else {
                return;
            };

            for capability in &packages[index].requires {
                visit(packages, &capability.name, selected, visited, ordered);
            }

            ordered.push(SelectedPackage {
                name: packages[index].name.clone(),
                version: packages[index].version.clone(),
            });
        }

        let mut visited = BTreeSet::new();
        let mut ordered = Vec::new();
        visit(packages, root, selected, &mut visited, &mut ordered);
        Self { packages: ordered }
    }
}

/// 求解失败的原因；所有变体都能说明“谁提出了什么要求”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SolveError {
    /// 根包或要求指向的包在仓库中完全不存在。
    PackageNotFound { package: String },
    /// 包存在，但没有一个版本满足该条 Capability。
    NoMatchingVersion {
        requester: String,
        capability: Capability,
        available: Vec<Version>,
    },
    /// 该包已选定版本，但后续 Capability 与它冲突。
    SelectedVersionConflict {
        requester: String,
        capability: Capability,
        selected: Version,
    },
}

impl fmt::Display for SolveError {
    /// 生成可直接展示给使用者的中文无解说明。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PackageNotFound { package } => {
                write!(formatter, "无解: 仓库中不存在包 `{package}`")
            }
            Self::NoMatchingVersion {
                requester,
                capability,
                available,
            } => {
                write!(
                    formatter,
                    "无解: 包 `{requester}` 依赖 `{capability}`"
                )?;
                if available.is_empty() {
                    write!(formatter, "，但仓库中不存在 `{}`", capability.name)
                } else {
                    let versions = available
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(
                        formatter,
                        "，但仓库中没有满足该约束的版本；`{}` 可用版本: {versions}",
                        capability.name
                    )
                }
            }
            Self::SelectedVersionConflict {
                requester,
                capability,
                selected,
            } => write!(
                formatter,
                "无解: 包 `{requester}` 依赖 `{capability}`，但已选择 `{}` {selected}，不满足该约束",
                capability.name
            ),
        }
    }
}

impl std::error::Error for SolveError {}

/// 一条待处理的 Capability 要求，带上提出者以便输出冲突信息。
#[derive(Clone, Debug)]
struct Requirement {
    requester: String,
    capability: Capability,
}

/// 在固定仓库上执行的玩具依赖求解器。
pub struct Solver<'a> {
    packages: &'a [Package],
}

impl<'a> Solver<'a> {
    /// 绑定一个仓库；求解过程不会修改仓库。
    pub fn new(packages: &'a [Package]) -> Self {
        Self { packages }
    }

    /// 求解根包及其全部传递依赖；成功时返回已排序的安装计划。
    pub fn resolve(&self, root: &str) -> Result<Resolution, SolveError> {
        let root_candidates = self.root_candidates(root);
        if root_candidates.is_empty() {
            return Err(SolveError::PackageNotFound {
                package: root.to_string(),
            });
        }

        let mut first_error = None;
        for root_index in root_candidates {
            let mut selected = BTreeMap::new();
            selected.insert(root.to_string(), root_index);

            match self.search(selected, self.requirements_of(root_index)) {
                Ok(selected) => return Ok(self.install_plan(root, &selected)),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        Err(first_error.expect("根包至少有一个候选版本，必然已记录失败原因"))
    }

    /// 回溯搜索：逐条满足 Capability，失败时尝试同一包的下一个较低版本。
    fn search(
        &self,
        selected: BTreeMap<String, usize>,
        mut pending: Vec<Requirement>,
    ) -> Result<BTreeMap<String, usize>, SolveError> {
        let Some(requirement) = pending.pop() else {
            return Ok(selected);
        };
        let Requirement {
            requester,
            capability,
        } = requirement;

        if let Some(&selected_index) = selected.get(&capability.name) {
            let selected_version = &self.packages[selected_index].version;
            if capability.matches(selected_version) {
                return self.search(selected, pending);
            }
            return Err(SolveError::SelectedVersionConflict {
                requester,
                capability,
                selected: selected_version.clone(),
            });
        }

        let candidates = self.candidates(&capability);
        if candidates.is_empty() {
            return Err(SolveError::NoMatchingVersion {
                requester,
                available: self.available_versions(&capability.name),
                capability,
            });
        }

        let mut first_error = None;
        for candidate_index in candidates {
            let mut next_selected = selected.clone();
            next_selected.insert(capability.name.clone(), candidate_index);

            let mut next_pending = pending.clone();
            next_pending.extend(self.requirements_of(candidate_index));

            match self.search(next_selected, next_pending) {
                Ok(resolved) => return Ok(resolved),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        Err(first_error.expect("候选非空，必然已记录失败原因"))
    }

    /// 返回仓库中某个包名的全部版本索引，版本从高到低排列。
    fn root_candidates(&self, name: &str) -> Vec<usize> {
        let mut indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| package.name == name)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        self.sort_newest_first(&mut indexes);
        indexes
    }

    /// 返回满足某条 Capability 的包索引，版本从高到低排列。
    fn candidates(&self, capability: &Capability) -> Vec<usize> {
        let mut indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| {
                package.name == capability.name && capability.matches(&package.version)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        self.sort_newest_first(&mut indexes);
        indexes
    }

    /// 把索引按对应包的版本从高到低排序。
    fn sort_newest_first(&self, indexes: &mut [usize]) {
        indexes.sort_by(|&left, &right| {
            self.packages[right]
                .version
                .cmp(&self.packages[left].version)
        });
    }

    /// 列出仓库中某个包的全部版本，供无解信息展示。
    fn available_versions(&self, name: &str) -> Vec<Version> {
        let mut versions = self
            .packages
            .iter()
            .filter(|package| package.name == name)
            .map(|package| package.version.clone())
            .collect::<Vec<_>>();
        versions.sort();
        versions
    }

    /// 把某个已选包声明的 Capability 要求转换成待处理要求。
    fn requirements_of(&self, index: usize) -> Vec<Requirement> {
        let package = &self.packages[index];
        package
            .requires
            .iter()
            .map(|capability| Requirement {
                requester: package.name.clone(),
                capability: capability.clone(),
            })
            .collect()
    }

    /// 对选定包做后序 DFS，得到“被依赖者在前”的安装顺序。
    fn install_plan(&self, root: &str, selected: &BTreeMap<String, usize>) -> Resolution {
        let mut visited = BTreeSet::new();
        let mut packages = Vec::new();
        self.visit(root, selected, &mut visited, &mut packages);
        Resolution { packages }
    }

    /// 递归访问依赖，在访问完所有依赖后才把当前包放入计划。
    fn visit(
        &self,
        name: &str,
        selected: &BTreeMap<String, usize>,
        visited: &mut BTreeSet<String>,
        packages: &mut Vec<SelectedPackage>,
    ) {
        if !visited.insert(name.to_string()) {
            return;
        }

        let Some(&index) = selected.get(name) else {
            return;
        };

        for capability in &self.packages[index].requires {
            self.visit(&capability.name, selected, visited, packages);
        }

        packages.push(SelectedPackage {
            name: self.packages[index].name.clone(),
            version: self.packages[index].version.clone(),
        });
    }
}
