$i18nDir = "D:\Progetti\Progetto_IAOS\AiPromptBuilder\desktop\ui\i18n"
$translationsFile = "C:\Users\TAKERO~1\AppData\Local\Temp\claude\d--Progetti-Progetto-IAOS-AiPromptBuilder\da5c045e-fb16-47a8-aee3-51ff784223a6\scratchpad\translations.json"

# Read translations
$translations = Get-Content $translationsFile | ConvertFrom-Json

# Get list of all language files except en.json and it.json
$langs = @('ar', 'bg', 'bho', 'bn', 'ca', 'cs', 'da', 'de', 'el', 'es', 'et', 'eu', 'fa', 'fi', 'fr', 'gu', 'he', 'hi', 'hr', 'hu', 'id', 'ja', 'kn', 'ko', 'lt', 'lv', 'ml', 'mr', 'nl', 'no', 'or', 'pa', 'pl', 'pt', 'ro', 'ru', 'sk', 'sl', 'sr', 'sv', 'sw', 'ta', 'te', 'th', 'tr', 'uk', 'ur', 'vi', 'zh')

foreach ($lang in $langs) {
    $filePath = Join-Path $i18nDir "$lang.json"
    
    # Read the JSON file
    $json = Get-Content $filePath | ConvertFrom-Json
    
    # Get translations for this language
    $langTranslations = $translations.$lang
    
    # Update the three keys
    $json.provColBar = $langTranslations.provColBar
    $json.provColDefault = $langTranslations.provColDefault
    $json.setProvidersBarHint2 = $langTranslations.setProvidersBarHint2
    
    # Convert back to JSON and save (preserving formatting)
    $json | ConvertTo-Json -Depth 100 | Set-Content $filePath -Encoding UTF8
    
    Write-Host "Updated $lang.json"
}

Write-Host "Done!"
