use super::*;

#[async_trait]
impl LexicalIndex for CozoStore {
    fn semantic_id(&self) -> &'static str {
        "mnestic-0.13-bm25-semantic-corpus-v2"
    }

    async fn search(&self, query: &str, k: usize, status: StatusFilter) -> Result<Vec<Scored>> {
        if k == 0 || (!status.active && !status.archived) {
            return Ok(Vec::new());
        }
        // Treat user text as text, never as native FTS syntax. Quoted analyzer
        // tokens joined by OR give BM25 bag-of-words recall while safely handling
        // punctuation, parens, and words such as AND/OR themselves.
        let terms: HashSet<String> = crate::lexical_terms(query).into_iter().collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let fts_query = terms
            .iter()
            .map(|term| serde_json::to_string(term).expect("serialize FTS token"))
            .collect::<Vec<_>>()
            .join(" OR ");

        let mut rules = Vec::new();
        if status.active {
            rules.push(
                "hit[id, score] := ~node_search:active_fts{id | query: $query, k: $k, bind_score: score}",
            );
        }
        if status.archived {
            rules.push(
                "hit[id, score] := ~node_search:archived_fts{id | query: $query, k: $k, bind_score: score}",
            );
        }
        let script = format!(
            "{}\n?[id, score] := hit[id, score] :order -score :limit $k",
            rules.join("\n")
        );
        let mut params = BTreeMap::new();
        params.insert("query".into(), dv_str(&fts_query));
        params.insert("k".into(), dv_int(k.min(i64::MAX as usize) as i64));
        let rows = self.run_async(script, params, false).await?;
        rows.rows
            .iter()
            .map(|row| {
                Ok(Scored {
                    id: node_id(want_str(&row[0])?)?,
                    score: want_f64(&row[1])? as f32,
                })
            })
            .collect()
    }
}
