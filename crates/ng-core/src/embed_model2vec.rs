//! Embedder semântico real usando `model2vec-rs` (embeddings estáticos da
//! MinishLab: tokeniza → lookup → mean-pool → PCA; só CPU, sem ONNX nem GPU;
//! modelo de 7–30MB, ~18µs por frase).
//!
//! Fica atrás da feature `model2vec` (desligada por padrão) para manter o
//! build padrão leve — a restrição dura do projeto é não pesar o PC do
//! usuário. Quando ligada, o crate é compilado com `local-only`, então
//! [`Model2VecEmbedder`] só carrega um modelo já presente no disco (nunca
//! baixa da rede): o diretório é apontado por `NG_EMBED_MODEL` ou, na
//! ausência dele, pelo cache padrão `~/.not-goldfish/models/<nome>`.

use model2vec_rs::model::StaticModel;

use crate::embed::Embedder;
use crate::{paths, Error, Result};

/// Nome do modelo procurado sob `~/.not-goldfish/models/` quando
/// `NG_EMBED_MODEL` não está definido. Um potion multilíngue cobre pt-BR.
const DEFAULT_MODEL_NAME: &str = "potion-multilingual-128M";

/// Embedder respaldado por um modelo model2vec estático carregado do disco.
pub struct Model2VecEmbedder {
    model: StaticModel,
    dim: usize,
    id: String,
}

impl Model2VecEmbedder {
    /// Carrega o modelo resolvido de `NG_EMBED_MODEL` (ou do cache padrão).
    /// Falha com `Err` se o diretório não existir ou o modelo for inválido —
    /// o chamador ([`crate::embed::default_embedder`]) faz fallback para o
    /// [`crate::HashEmbedder`] em vez de entrar em pânico.
    pub fn from_env() -> Result<Self> {
        let dir = model_dir();
        if !dir.exists() {
            return Err(Error::Other(format!(
                "modelo model2vec não encontrado em {} (defina NG_EMBED_MODEL)",
                dir.display()
            )));
        }
        Self::load(dir)
    }

    /// Carrega de um diretório específico. `normalize = Some(true)` força
    /// vetores unitários (L2) independentemente do config do modelo, para que
    /// o cosseno case com a convenção do [`crate::HashEmbedder`].
    pub fn load(dir: impl AsRef<std::path::Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let model = StaticModel::from_pretrained(dir, None, Some(true), None)
            .map_err(|e| Error::Other(format!("falha ao carregar model2vec: {e}")))?;

        // O crate não expõe a dimensão diretamente; medimos codificando uma
        // sonda curta e lendo o tamanho do vetor resultante.
        let dim = model.encode_single("dim").len();
        if dim == 0 {
            return Err(Error::Other(
                "modelo model2vec retornou vetor de dimensão zero".into(),
            ));
        }

        // O id carrega nome + dimensão. É a chave `model` sob a qual os
        // vetores são gravados na tabela `embeddings`: modelos ou dimensões
        // distintos nunca compartilham a mesma chave, então o rerank híbrido
        // (`Store::search_hybrid`, que filtra `WHERE model = embedder.id()`)
        // jamais lê um vetor de dimensão incompatível para o cosseno — esta é
        // a garantia de segurança de dimensão, sem tocar no schema.
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("desconhecido");
        let id = format!("m2v-{name}-{dim}");

        Ok(Self { model, dim, id })
    }
}

impl Embedder for Model2VecEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        // Texto vazio → vetor zero (como o HashEmbedder), para o cosseno
        // tratá-lo como "não relacionado" em vez de produzir NaN.
        if text.trim().is_empty() {
            return vec![0.0; self.dim];
        }
        self.model.encode_single(text)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        &self.id
    }
}

/// Diretório do modelo: `NG_EMBED_MODEL` se definido, senão o cache padrão
/// `~/.not-goldfish/models/<DEFAULT_MODEL_NAME>`.
fn model_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("NG_EMBED_MODEL") {
        return std::path::PathBuf::from(dir);
    }
    paths::data_dir().join("models").join(DEFAULT_MODEL_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::cosine;

    #[test]
    fn model_dir_respeita_env_e_cai_no_cache_padrao() {
        // Com NG_EMBED_MODEL definido, o caminho é usado literalmente.
        std::env::set_var("NG_EMBED_MODEL", "/caminho/explicito/modelo");
        assert_eq!(
            model_dir(),
            std::path::PathBuf::from("/caminho/explicito/modelo")
        );

        // Sem a env, cai no cache padrão sob data_dir()/models/<nome>.
        std::env::remove_var("NG_EMBED_MODEL");
        let d = model_dir();
        assert!(
            d.ends_with(std::path::Path::new("models").join(DEFAULT_MODEL_NAME)),
            "esperava terminar em models/{DEFAULT_MODEL_NAME}, achei {}",
            d.display()
        );
    }

    // Prova de qualidade real: só roda quando `NG_EMBED_MODEL` aponta para um
    // modelo model2vec de verdade. Rode com:
    //   cargo test -p ng-core --features model2vec -- --ignored
    #[test]
    #[ignore = "requer um modelo model2vec real em NG_EMBED_MODEL"]
    fn frases_relacionadas_em_pt_br_tem_cosseno_maior() {
        let e = Model2VecEmbedder::from_env().expect("modelo deve carregar");
        let a = e.embed("corrigir bug de login");
        let b = e.embed("erro de autenticação no acesso");
        let c = e.embed("configurar cache redis");

        let relacionadas = cosine(&a, &b);
        let dispares = cosine(&a, &c);
        assert!(
            relacionadas > dispares,
            "esperado {relacionadas} > {dispares} (frases relacionadas vs. díspares)"
        );
    }

    #[test]
    #[ignore = "requer um modelo model2vec real em NG_EMBED_MODEL"]
    fn texto_vazio_e_vetor_zero_com_dimensao_correta() {
        let e = Model2VecEmbedder::from_env().expect("modelo deve carregar");
        let v = e.embed("   ");
        assert_eq!(v.len(), e.dim());
        assert!(v.iter().all(|x| *x == 0.0));
        assert_eq!(cosine(&v, &v), 0.0);
    }
}
