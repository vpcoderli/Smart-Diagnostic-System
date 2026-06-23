use anyhow::{anyhow, Result};
use async_trait::async_trait;
use diag_core::collector_trait::ServiceDiscovery;
use diag_core::config::NacosConfig;
use diag_core::models::ServiceInstance;
use reqwest::Client;
use serde::Deserialize;

pub struct NacosDiscovery {
    config: NacosConfig,
    client: Client,
}

#[derive(Deserialize)]
struct NacosServiceList {
    doms: Option<Vec<String>>,
    services: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct NacosInstanceList {
    hosts: Vec<NacosInstance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NacosInstance {
    ip: String,
    port: u16,
    healthy: bool,
}

fn infer_log_path(service_name: &str, pattern: &str) -> String {
    pattern.replace("{service-name}", service_name)
}

impl NacosDiscovery {
    pub fn new(config: NacosConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl ServiceDiscovery for NacosDiscovery {
    async fn discover_services(&self, prefix: &str) -> Result<Vec<ServiceInstance>> {
        let list_url = format!(
            "{}/nacos/v1/ns/service/list?pageNo=1&pageSize=100&namespaceId={}&groupName={}",
            self.config.address.trim_end_matches('/'),
            self.config.namespace,
            self.config.group
        );

        let resp = self
            .client
            .get(&list_url)
            .send()
            .await
            .map_err(|e| anyhow!("Nacos 连接失败: {}", e))?;
        let list: NacosServiceList = resp
            .json()
            .await
            .map_err(|e| anyhow!("Nacos 服务列表解析失败: {}", e))?;

        let service_names: Vec<String> = list
            .doms
            .or(list.services)
            .unwrap_or_default()
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .collect();

        tracing::info!("Nacos 发现 {} 个 {} 服务", service_names.len(), prefix);

        let mut instances = Vec::new();
        for svc_name in &service_names {
            let inst_url = format!(
                "{}/nacos/v1/ns/instance/list?serviceName={}&namespaceId={}&groupName={}&healthyOnly=true",
                self.config.address.trim_end_matches('/'),
                svc_name,
                self.config.namespace,
                self.config.group
            );

            match self.client.get(&inst_url).send().await {
                Ok(resp) => {
                    if let Ok(inst_list) = resp.json::<NacosInstanceList>().await {
                        for inst in inst_list.hosts {
                            instances.push(ServiceInstance {
                                service_name: svc_name.clone(),
                                ip: inst.ip,
                                port: inst.port,
                                healthy: inst.healthy,
                                log_dir: infer_log_path(svc_name, &self.config.log_path_pattern),
                                log_pattern: "*.log".to_string(),
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Nacos 查询服务 {} 实例失败: {}", svc_name, e);
                }
            }
        }

        Ok(instances)
    }

    fn source_type(&self) -> &'static str {
        "nacos"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_log_path() {
        assert_eq!(
            infer_log_path("pcm-management", "/var/log/{service-name}/"),
            "/var/log/pcm-management/"
        );
    }

    #[test]
    fn test_infer_log_path_no_placeholder() {
        assert_eq!(
            infer_log_path("pcm-management", "/data/logs/"),
            "/data/logs/"
        );
    }
}
