use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;
use std::ops::Range;
use std::str::FromStr;
use std::sync::Arc;
use itertools::Itertools;

use builtin_entities::{BuiltinEntityKind, RustlingParser};
use errors::*;
use pipeline::{IntentParser, IntentParserResult, Slot, SlotValue};
use pipeline::rule_based::RuleBasedIntentParser;
use pipeline::probabilistic::ProbabilisticIntentParser;
use pipeline::tagging_utils::{enrich_entities, tag_builtin_entities, disambiguate_tagged_entities};
use pipeline::configuration::{Entity, NLUEngineConfigurationConvertible};
use rustling_ontology::Lang;
use utils::token::{tokenize, compute_all_ngrams};
use utils::string::substring_with_char_range;

const MODEL_VERSION: &str = "0.8.3";

pub struct SnipsNLUEngine {
    language: String,
    parsers: Vec<Box<IntentParser>>,
    entities: HashMap<String, Entity>,
    intents_data_sizes: HashMap<String, usize>,
    slot_name_mapping: HashMap<String, HashMap<String, String>>,
    builtin_entity_parser: Arc<RustlingParser>
}

impl SnipsNLUEngine {
    pub fn new<T: NLUEngineConfigurationConvertible + 'static>(configuration: T) -> Result<Self> {
        let nlu_config = configuration.into_nlu_engine_configuration();

        let mut parsers: Vec<Box<IntentParser>> = Vec::with_capacity(2);

        let model = nlu_config.model;
        if let Some(config) = model.rule_based_parser {
            parsers.push(Box::new(RuleBasedIntentParser::new(config)?))
        };
        if let Some(config) = model.probabilistic_parser {
            parsers.push(Box::new(ProbabilisticIntentParser::new(config)?))
        };
        let intents_data_sizes = nlu_config.intents_data_sizes;
        let slot_name_mapping = nlu_config.slot_name_mapping;
        let rustling_lang = Lang::from_str(&nlu_config.language)?;
        Ok(SnipsNLUEngine {
            language: nlu_config.language,
            parsers,
            entities: nlu_config.entities,
            intents_data_sizes,
            slot_name_mapping,
            builtin_entity_parser: RustlingParser::get(rustling_lang)
        })
    }

    pub fn parse(&self, input: &str, intents_filter: Option<&[&str]>) -> Result<IntentParserResult> {
        if self.parsers.is_empty() {
            return Ok(IntentParserResult { input: input.to_string(), intent: None, slots: None });
        }
        let set_intents: Option<HashSet<String>> = intents_filter.map(|intent_list|
            HashSet::from_iter(intent_list.iter().map(|name| name.to_string()))
        );

        for parser in self.parsers.iter() {
            let classification_result = parser.get_intent(input, set_intents.as_ref())?;
            if let Some(classification_result) = classification_result {
                let valid_slots = parser
                    .get_slots(input, &classification_result.intent_name)?
                    .into_iter()
                    .filter_map(|slot| {
                        if let Some(entity) = self.entities.get(&slot.entity) {
                            entity.utterances
                                .get(&slot.raw_value)
                                .map(|reference_value|
                                    Some(slot.clone().with_slot_value(SlotValue::Custom(reference_value.to_string()))))
                                .unwrap_or(
                                    if entity.automatically_extensible {
                                        Some(slot)
                                    } else {
                                        None
                                    }
                                )
                        } else {
                            Some(slot)
                        }
                    })
                    .collect();

                return Ok(
                    IntentParserResult {
                        input: input.to_string(),
                        intent: Some(classification_result),
                        slots: Some(valid_slots)
                    }
                )
            }
        }
        Ok(IntentParserResult { input: input.to_string(), intent: None, slots: None })
    }

    pub fn model_version() -> &'static str {
        MODEL_VERSION
    }
}

impl SnipsNLUEngine {
    pub fn extract_slot(&self, input: String, intent_name: &str, slot_name: String) -> Result<Option<Slot>> {
        let entity_name = self.slot_name_mapping
            .get(intent_name)
            .ok_or(format!("Unknown intent: {}", intent_name))?
            .get(&slot_name)
            .ok_or(format!("Unknown slot: {}", &slot_name))?;

        let slot = if let Some(custom_entity) = self.entities.get(entity_name) {
            extract_custom_entity(input.to_string(),
                                  entity_name.to_string(),
                                  slot_name.to_string(),
                                  custom_entity.clone())
        } else {
            extract_builtin_entity(input,
                                   entity_name.to_string(),
                                   slot_name.to_string(),
                                   self.builtin_entity_parser.clone())?
        };
        Ok(slot)
    }
}

fn extract_custom_entity(input: String,
                         entity_name: String,
                         slot_name: String,
                         custom_entity: Entity) -> Option<Slot> {
    custom_entity.utterances
        .get(&input)
        .map(|reference_value|
            Some(
                Slot {
                    raw_value: input.clone(),
                    value: SlotValue::Custom(reference_value.to_string()),
                    range: None,
                    entity: entity_name.clone(),
                    slot_name: slot_name.clone()
                }))
        .unwrap_or(
            if custom_entity.automatically_extensible {
                Some(
                    Slot {
                        raw_value: input.clone(),
                        value: SlotValue::Custom(input),
                        range: None,
                        entity: entity_name,
                        slot_name: slot_name
                    })
            } else {
                None
            })
}

fn extract_builtin_entity(input: String,
                          entity_name: String,
                          slot_name: String,
                          builtin_entity_parser: Arc<RustlingParser>) -> Result<Option<Slot>> {
    let builtin_entity_kind = BuiltinEntityKind::from_identifier(&entity_name)?;
    Ok(builtin_entity_parser
        .extract_entities(&input, Some(&vec![builtin_entity_kind]))
        .first()
        .map(|rustlin_entity|
            Slot {
                raw_value: substring_with_char_range(input, &rustlin_entity.range),
                value: SlotValue::Builtin(rustlin_entity.entity.clone()),
                range: None,
                entity: entity_name,
                slot_name: slot_name
            }
        ))
}

const DEFAULT_THRESHOLD: usize = 5;


#[derive(Serialize, Debug, Clone, PartialEq, Hash)]
pub struct TaggedEntity {
    pub value: String,
    pub range: Option<Range<usize>>,
    pub entity: String,
    pub slot_name: Option<String>
}

impl SnipsNLUEngine {
    pub fn tag(&self,
               text: &str,
               intent: &str,
               small_data_regime_threshold: Option<usize>) -> Result<Vec<TaggedEntity>> {
        let intent_data_size: usize = *self.intents_data_sizes
            .get(intent)
            .ok_or(format!("Unknown intent: {}", intent))?;
        let slot_name_mapping = self.slot_name_mapping
            .get(intent)
            .ok_or(format!("Unknown intent: {}", intent))?;
        let intent_entities = HashSet::from_iter(slot_name_mapping.values());
        let threshold = small_data_regime_threshold.unwrap_or(DEFAULT_THRESHOLD);
        let parsed_entities = self.parse(text, Some(&vec![intent]))?
            .slots
            .map(|slots|
                slots.into_iter()
                    .map(|s| TaggedEntity {
                        value: s.raw_value,
                        range: s.range,
                        entity: s.entity,
                        slot_name: Some(s.slot_name)
                    })
                    .collect_vec())
            .unwrap_or(vec![]);

        if intent_data_size >= threshold {
            return Ok(parsed_entities);
        }

        let tagged_seen_entities = self.tag_seen_entities(text, intent_entities);
        let tagged_builtin_entities = tag_builtin_entities(text, &self.language);
        let mut tagged_entities = enrich_entities(tagged_seen_entities, tagged_builtin_entities);
        tagged_entities = enrich_entities(tagged_entities, parsed_entities);
        Ok(disambiguate_tagged_entities(tagged_entities, slot_name_mapping.clone()))
    }

    fn tag_seen_entities(&self, text: &str, intent_entities: HashSet<&String>) -> Vec<TaggedEntity> {
        let entities = self.entities.clone().into_iter()
            .filter_map(|(entity_name, entity)|
                if intent_entities.contains(&entity_name) {
                    Some((entity_name, entity))
                } else {
                    None
                })
            .collect_vec();
        let tokens = tokenize(text);
        let token_values_ref = tokens.iter().map(|v| &*v.value).collect_vec();
        let mut ngrams = compute_all_ngrams(&*token_values_ref, tokens.len());
        ngrams.sort_by_key(|&(_, ref indexes)| -(indexes.len() as i16));
        let mut tagged_entities = Vec::<TaggedEntity>::new();
        for (ngram, ngram_indexes) in ngrams {
            let mut ngram_entity: Option<TaggedEntity> = None;
            for &(ref entity_name, ref entity_data) in entities.iter() {
                if entity_data.utterances.contains_key(&ngram) {
                    if ngram_entity.is_some() {
                        // If the ngram matches several entities, i.e. there is some ambiguity, we
                        // don't add it to the tagged entities
                        ngram_entity = None;
                        break;
                    }
                    if let (Some(first), Some(last)) = (ngram_indexes.first(), ngram_indexes.last()) {
                        let range_start = tokens[*first].char_range.start;
                        let range_end = tokens[*last].char_range.end;
                        let range = range_start..range_end;
                        let value = substring_with_char_range(text.to_string(), &range);
                        ngram_entity = Some(TaggedEntity {
                            value,
                            range: Some(range),
                            entity: entity_name.to_string(),
                            slot_name: None
                        })
                    }
                }
            }
            if let Some(ngram_entity) = ngram_entity {
                tagged_entities = enrich_entities(tagged_entities, vec![ngram_entity])
            }
        }
        tagged_entities
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use pipeline::configuration::NLUEngineConfiguration;
    use builtin_entities::BuiltinEntity;
    use builtin_entities::ontology::NumberValue;
    use pipeline::{IntentClassifierResult, Slot, SlotValue};
    use utils::miscellaneous::parse_json;

    #[test]
    fn it_works() {
        // Given
        let configuration: NLUEngineConfiguration = parse_json("tests/configurations/beverage_engine.json");
        let nlu_engine = SnipsNLUEngine::new(configuration).unwrap();

        // When
        let result = nlu_engine.parse("Make me two cups of coffee please", None).unwrap();

        // Then
        let expected_entity_value = SlotValue::Builtin(BuiltinEntity::Number(NumberValue(2.0)));
        let expected_result = IntentParserResult {
            input: "Make me two cups of coffee please".to_string(),
            intent: Some(IntentClassifierResult {
                intent_name: "MakeCoffee".to_string(),
                probability: 0.7035172
            }),
            slots: Some(vec![Slot {
                raw_value: "two".to_string(),
                value: expected_entity_value,
                range: Some(8..11),
                entity: "snips/number".to_string(),
                slot_name: "number_of_cups".to_string()
            }])
        };
        assert_eq!(expected_result, result)
    }
}