//! A small culture table shared by unit tests that need a
//! `LocalizationManager` resolving English and French tokens.

/// A culture table holding English (`eng`) and French (`fre`, `fra`), and
/// the US ratings `G`, `PG` and `R`.
pub(crate) struct EnglishAndFrench;

impl ferrofin_traits::localization::LocalizationManager for EnglishAndFrench {
    fn get_cultures(&self) -> Vec<ferrofin_model::globalization::CultureDto> {
        let culture =
            |two: &str, display: &str, three: &[&str]| ferrofin_model::globalization::CultureDto {
                name: two.to_owned(),
                display_name: display.to_owned(),
                two_letter_iso_language_name: two.to_owned(),
                three_letter_iso_language_name: three.first().map(|t| (*t).to_owned()),
                three_letter_iso_language_names: three.iter().map(|t| (*t).to_owned()).collect(),
            };
        vec![
            culture("en", "English", &["eng"]),
            culture("fr", "French", &["fre", "fra"]),
        ]
    }
    fn get_countries(&self) -> Vec<ferrofin_model::globalization::CountryInfo> {
        unimplemented!()
    }
    fn get_parental_ratings(&self) -> Vec<ferrofin_model::entities_media::ParentalRating> {
        unimplemented!()
    }
    fn get_localization_options(&self) -> Vec<ferrofin_model::globalization::LocalizationOption> {
        unimplemented!()
    }
    fn get_localized_string(&self, _phrase: &str) -> String {
        unimplemented!()
    }
    fn get_server_localized_string(&self, _phrase: &str) -> String {
        unimplemented!()
    }
    fn get_localized_string_for(&self, _phrase: &str, _culture: &str) -> String {
        unimplemented!()
    }
    fn find_language_info(
        &self,
        language: &str,
    ) -> Option<ferrofin_model::globalization::CultureDto> {
        self.get_cultures().into_iter().find(|culture| {
            language.eq_ignore_ascii_case(&culture.display_name)
                || language.eq_ignore_ascii_case(&culture.two_letter_iso_language_name)
                || culture
                    .three_letter_iso_language_names
                    .iter()
                    .any(|code| language.eq_ignore_ascii_case(code))
        })
    }
    fn get_language_display_name(&self, _language: &str) -> Option<String> {
        unimplemented!()
    }
    fn get_rating_score(
        &self,
        rating: &str,
        _country_code: Option<&str>,
    ) -> Option<ferrofin_model::entities_media::ParentalRatingScore> {
        let score = match rating {
            "G" => 0,
            "PG" => 10,
            "R" => 17,
            _ => return None,
        };
        Some(ferrofin_model::entities_media::ParentalRatingScore {
            score,
            sub_score: None,
        })
    }
}
